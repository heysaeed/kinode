use crate::kernel::{ProcessMessageReceiver, ProcessMessageSender};
use crate::KERNEL_PROCESS_ID;
use anyhow::Result;
use lib::types::core as t;
pub use lib::wit;
pub use lib::wit::Host as StandardHost;
pub use lib::Process;
use ring::signature::{self, KeyPair};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::task::JoinHandle;
use wasmtime::component::*;
use wasmtime::{Engine, Store};
use wasmtime_wasi::preview2::{pipe::MemoryOutputPipe, Table, WasiCtx, WasiCtxBuilder, WasiView};

const STACK_TRACE_SIZE: usize = 5000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessContext {
    // store ultimate in order to set prompting message if needed
    pub message: Option<KernelMessage>,
    // can be empty if a request doesn't set context, but still needs to inherit
    pub context: Option<Context>,
}

pub struct ProcessState {
    /// our node's networking keypair
    pub keypair: Arc<signature::Ed25519KeyPair>,
    /// information about ourself
    pub metadata: t::ProcessMetadata,
    /// pipe from which we get messages from the main event loop
    pub recv_in_process: ProcessMessageReceiver,
    /// pipe to send messages to ourself (received in `recv_in_process`)
    pub self_sender: ProcessMessageSender,
    /// pipe for sending messages to the main event loop
    pub send_to_loop: t::MessageSender,
    /// pipe for sending [`t::Printout`]s to the terminal
    pub send_to_terminal: t::PrintSender,
    /// store the nested request, if any
    pub nested_request: Option<t::KernelMessage>,
    /// store the current incoming message that we've gotten from receive()
    pub current_incoming_message: Option<t::KernelMessage>,
    /// store the contexts and timeout task of all outstanding requests
    pub contexts: HashMap<u64, (t::ProcessContext, JoinHandle<()>)>,
    /// store the messages that we've gotten from event loop but haven't processed yet
    /// TODO make this an ordered map for O(1) retrieval by ID
    pub message_queue: VecDeque<Result<t::KernelMessage, t::WrappedSendError>>,
    /// pipe for getting info about capabilities
    pub caps_oracle: t::CapMessageSender,
}

pub struct ProcessWasi {
    pub process: ProcessState,
    table: Table,
    wasi: WasiCtx,
}

impl WasiView for ProcessWasi {
    fn table(&self) -> &Table {
        &self.table
    }
    fn table_mut(&mut self) -> &mut Table {
        &mut self.table
    }
    fn ctx(&self) -> &WasiCtx {
        &self.wasi
    }
    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

pub async fn send_and_await_response(
    process: &mut ProcessWasi,
    source: Option<t::Address>,
    target: wit::Address,
    request: wit::Request,
    blob: Option<wit::LazyLoadBlob>,
) -> Result<Result<(wit::Address, wit::Message), wit::SendError>> {
    if request.expects_response.is_none() {
        return Err(anyhow::anyhow!(
            "kernel: got invalid send_and_await_response() Request from {:?}: must expect response",
            process.process.metadata.our.process
        ));
    }
    let id = process
        .process
        .send_request(source, target, request, None, blob)
        .await;
    match id {
        Ok(id) => match process.process.get_specific_message_for_process(id).await {
            Ok((address, wit::Message::Response(response))) => {
                Ok(Ok((address, wit::Message::Response(response))))
            }
            Ok((_address, wit::Message::Request(_))) => Err(anyhow::anyhow!(
                "fatal: received Request instead of Response"
            )),
            Err((net_err, _context)) => Ok(Err(net_err)),
        },
        Err(e) => Err(e),
    }
}

impl ProcessState {
    /// Ingest latest message directed to this process, and save it as the current message.
    /// If there is no message in the queue, wait async until one is received.
    pub async fn get_next_message_for_process(
        &mut self,
    ) -> Result<(wit::Address, wit::Message), (wit::SendError, Option<wit::Context>)> {
        let res = match self.message_queue.pop_front() {
            Some(message_from_queue) => message_from_queue,
            None => self
                .recv_in_process
                .recv()
                .await
                .expect("fatal: process couldn't receive next message"),
        };
        self.kernel_message_to_process_receive(res)
    }

    /// instead of ingesting latest, wait for a specific ID and queue all others
    async fn get_specific_message_for_process(
        &mut self,
        awaited_message_id: u64,
    ) -> Result<(wit::Address, wit::Message), (wit::SendError, Option<wit::Context>)> {
        // first, check if the awaited message is already in the queue and handle if so
        for (i, message) in self.message_queue.iter().enumerate() {
            match message {
                Ok(ref km) if km.id == awaited_message_id => {
                    let km = self.message_queue.remove(i).unwrap();
                    return self.kernel_message_to_process_receive(km);
                }
                _ => continue,
            }
        }
        // next, wait for the awaited message to arrive
        loop {
            let res = self
                .recv_in_process
                .recv()
                .await
                .expect("fatal: process couldn't receive next message");
            let id = match &res {
                Ok(km) => km.id,
                Err(e) => e.id,
            };
            if id == awaited_message_id {
                return self.kernel_message_to_process_receive(res);
            } else {
                self.message_queue.push_back(res);
            }
        }
    }

    /// takes Request generated by a process and sends it to the main event loop.
    /// will only fail if process does not have capability to send to target.
    /// if the request has a timeout (expects response), start a task to track
    /// that timeout and return timeout error if it expires.
    pub async fn send_request(
        &mut self,
        /// only used when kernel steps in to get/set state
        fake_source: Option<t::Address>,
        target: wit::Address,
        request: wit::Request,
        new_context: Option<wit::Context>,
        blob: Option<wit::LazyLoadBlob>,
    ) -> Result<u64> {
        let source = fake_source.unwrap_or(self.metadata.our.clone());
        let mut request = t::de_wit_request(request);

        // if request chooses to inherit, it means to take the lazy_load_blob, if any,
        // from the last request it ingested. if current_incoming_message is a request,
        // it will be the inherited one. if not, we check nested_request and inherit
        // from that if it exists.
        // if neither exist as requests, inherit flag will be ignored.
        let predecessor_request = match &self.current_incoming_message {
            Some(t::KernelMessage {
                message: t::Message::Request(request),
                ..
            }) => self.current_incoming_message.as_ref(),
            _ => match &self.nested_request {
                Some(t::KernelMessage {
                    message: t::Message::Request(request),
                    ..
                }) => self.nested_request.as_ref(),
                _ => None,
            },
        };

        // if request chooses to inherit, match id to precedessor
        // otherwise, id is generated randomly
        let request_id: u64 = if request.inherit && predecessor_request.is_some() {
            predecessor_request.unwrap().id
        } else {
            loop {
                let id = rand::random();
                if !self.contexts.contains_key(&id) {
                    break id;
                }
            }
        };

        // if a blob is provided, it will be used; otherwise, if inherit is true,
        // and a predecessor exists, its blob will be used; otherwise, no blob will be used.
        let blob = match blob {
            Some(p) => Some(t::LazyLoadBlob {
                mime: p.mime,
                bytes: p.bytes,
            }),
            None => match request.inherit {
                true => predecessor_request
                    .and_then(|km| km.lazy_load_blob.clone())
                    .or(None),
                false => None,
            },
        };

        if !request.capabilities.is_empty() {
            request.capabilities = {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.caps_oracle
                    .send(t::CapMessage::FilterCaps {
                        on: self.metadata.our.process.clone(),
                        caps: request
                            .capabilities
                            .iter()
                            .map(|cap| t::de_wit_capability(cap.clone()).0)
                            .collect(),
                        responder: tx,
                    })
                    .await?;
                rx.await?
            };
        }

        // if the request expects a response, modify the process' context map as needed
        // and set a timer.
        // TODO optimize this SIGNIFICANTLY: stop spawning tasks
        // and use a global clock + garbage collect step to check for timeouts
        if let Some(timeout_secs) = request.expects_response {
            let self_sender = self.self_sender.clone();
            let original_target = t::Address::de_wit(target.clone());
            let timeout_handle = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
                let _ = self_sender
                    .send(Err(t::WrappedSendError {
                        id: request_id,
                        source: original_target.clone(),
                        error: t::SendError {
                            kind: t::SendErrorKind::Timeout,
                            target: original_target,
                            message: t::Message::Request(request.clone()),
                            lazy_load_blob: blob.clone(),
                        },
                    }))
                    .await;
            });
            self.contexts.insert(
                request_id,
                (
                    ProcessContext {
                        message: predecessor_request.cloned(),
                        context: new_context,
                    },
                    timeout_handle,
                ),
            );
        }

        // rsvp is set based on this priority:
        // 1. whether this request expects a response -- if so, rsvp = our address, always
        // 2. whether this request inherits -- if so, rsvp = prompting message's rsvp
        // 3. if neither, rsvp = None
        let kernel_message = t::KernelMessage {
            id: request_id,
            source,
            target: t::Address::de_wit(target),
            rsvp: match (
                request.expects_response,
                request.inherit,
                &self.current_incoming_message,
            ) {
                (Some(_), _, _) => {
                    // this request expects response, so receives any response
                    // make sure to use the real source, not a fake injected-by-kernel source
                    Some(self.metadata.our.clone())
                }
                (None, true, Some(ref prompt)) => {
                    // this request inherits, so response will be routed to prompting message
                    prompt.rsvp.clone()
                }
                _ => None,
            },
            message: t::Message::Request(request),
            lazy_load_blob: blob,
        };

        self.send_to_loop
            .send(kernel_message)
            .await
            .expect("fatal: kernel couldn't send request");

        Ok(request_id)
    }

    /// takes Response generated by a process and sends it to the main event loop.
    pub async fn send_response(
        &mut self,
        response: wit::Response,
        blob: Option<wit::LazyLoadBlob>,
    ) {
        let (id, target) = match self.make_response_id_target().await {
            Some(r) => r,
            None => {
                let _ = self
                    .send_to_terminal
                    .send(t::Printout {
                        verbosity: 2,
                        content: format!("kernel: dropping Response {:?}", response),
                    })
                    .await;
                return;
            }
        };

        let blob = match response.inherit {
            true => self.last_blob.clone(),
            false => t::de_wit_blob(blob),
        };

        let mut inner_response = t::de_wit_response(response.clone());

        inner_response.capabilities = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = self
                .caps_oracle
                .send(t::CapMessage::FilterCaps {
                    on: self.metadata.our.process.clone(),
                    caps: response
                        .capabilities
                        .iter()
                        .map(|cap| t::de_wit_capability(cap.clone()).0)
                        .collect(),
                    responder: tx,
                })
                .await;
            rx.await.expect("fatal: process couldn't get caps")
        };

        self.send_to_loop
            .send(t::KernelMessage {
                id,
                source: self.metadata.our.clone(),
                target,
                rsvp: None,
                message: t::Message::Response((
                    inner_response,
                    // the context will be set by the process receiving this Response.
                    None,
                )),
                lazy_load_blob: blob,
            })
            .await
            .expect("fatal: kernel couldn't send response");
    }

    /// Convert a message from the main event loop into a result for the process to receive.
    /// If the message is a response or error, get context if we have one.
    /// If there exists a current message before this, if it is a Request, it will get
    /// saved as nested_request.
    fn kernel_message_to_process_receive(
        &mut self,
        res: Result<t::KernelMessage, t::WrappedSendError>,
    ) -> Result<(wit::Address, wit::Message), (wit::SendError, Option<wit::Context>)> {
        let (context, km) = match res {
            Ok(km) => match &km.message {
                t::Message::Request(_) => {
                    self.last_blob = km.lazy_load_blob.clone();
                    self.current_incoming_message = Some(km.clone());
                    (None, km)
                }
                t::Message::Response(_) => {
                    if let Some((context, timeout_handle)) = self.contexts.remove(&km.id) {
                        timeout_handle.abort();
                        self.last_blob = km.lazy_load_blob.clone();
                        self.current_incoming_message = match context.prompting_message {
                            None => Some(km.clone()),
                            Some(prompting_message) => Some(prompting_message),
                        };
                        (context.context, km)
                    } else {
                        self.last_blob = km.lazy_load_blob.clone();
                        self.current_incoming_message = Some(km.clone());
                        (None, km)
                    }
                }
            },
            Err(e) => match self.contexts.remove(&e.id) {
                None => return Err((t::en_wit_send_error(e.error), None)),
                Some((context, timeout_handle)) => {
                    timeout_handle.abort();
                    self.current_incoming_message = context.prompting_message;
                    return Err((t::en_wit_send_error(e.error), context.context));
                }
            },
        };

        let pk = signature::UnparsedPublicKey::new(
            &signature::ED25519,
            self.keypair.as_ref().public_key(),
        );

        Ok((
            km.source.en_wit(),
            match km.message {
                t::Message::Request(mut request) => {
                    // prune any invalid caps before sending
                    request.capabilities = request
                        .capabilities
                        .iter()
                        .filter_map(|(cap, sig)| {
                            // The only time we verify a cap's signature is when a foreign node
                            // sends us a cap that we (allegedly) issued
                            if km.source.node != self.metadata.our.node
                                && cap.issuer.node == self.metadata.our.node
                            {
                                match pk.verify(&rmp_serde::to_vec(&cap).unwrap_or_default(), sig) {
                                    Ok(_) => Some((cap.clone(), sig.clone())),
                                    Err(_) => None,
                                }
                            } else {
                                return Some((cap.clone(), sig.clone()));
                            }
                        })
                        .collect::<Vec<(t::Capability, Vec<u8>)>>();
                    wit::Message::Request(t::en_wit_request(request))
                }
                // NOTE: we throw away whatever context came from the sender, that's not ours
                t::Message::Response((mut response, _context)) => {
                    // prune any invalid caps before sending
                    response.capabilities = response
                        .capabilities
                        .iter()
                        .filter_map(|(cap, sig)| {
                            // The only time we verify a cap's signature is when a foreign node
                            // sends us a cap that we (allegedly) issued
                            if km.source.node != self.metadata.our.node
                                && cap.issuer.node == self.metadata.our.node
                            {
                                match pk.verify(&rmp_serde::to_vec(&cap).unwrap_or_default(), sig) {
                                    Ok(_) => Some((cap.clone(), sig.clone())),
                                    Err(_) => None,
                                }
                            } else {
                                return Some((cap.clone(), sig.clone()));
                            }
                        })
                        .collect::<Vec<(t::Capability, Vec<u8>)>>();
                    wit::Message::Response((t::en_wit_response(response), context))
                }
            },
        ))
    }

    /// Given the current process state, return the id and target that
    /// a response it emits should have. This takes into
    /// account the `rsvp` of the current or nested message, if any.
    async fn make_response_id_target(&self) -> Option<(u64, t::Address)> {
        let Some(ref current_incoming_message) = self.current_incoming_message else {
            println!("need non-None incoming_message to handle Response\r");
            return None;
        };
        match current_incoming_message.message {
            t::Message::Request(ref request) => {
                // if the current message is a Request, the response will always go there.
                Some((
                    current_incoming_message.id,
                    match &current_incoming_message.rsvp {
                        None => current_incoming_message.source.clone(),
                        Some(address) => address.clone(),
                    },
                ))
            }
            t::Message::Response((ref response, ref _maybe_context)) => {
                // if the current message is a Response, we look at the nested request
                // to see where the response should go.
                match &self.nested_request {
                    None => {
                        println!(
                            "need non-None incoming message or nested_request to handle Response\r"
                        );
                        return None;
                    }
                    Some(ref nested) => Some((
                        nested.id,
                        match &nested.rsvp {
                            None => nested.source.clone(),
                            Some(address) => address.clone(),
                        },
                    )),
                }
            }
        }
    }
}

/// create a specific process, and generate a task that will run it.
pub async fn make_process_loop(
    keypair: Arc<signature::Ed25519KeyPair>,
    metadata: t::ProcessMetadata,
    send_to_loop: t::MessageSender,
    send_to_terminal: t::PrintSender,
    mut recv_in_process: ProcessMessageReceiver,
    send_to_process: ProcessMessageSender,
    wasm_bytes: Vec<u8>,
    caps_oracle: t::CapMessageSender,
    engine: Engine,
) -> Result<()> {
    // before process can be instantiated, need to await 'run' message from kernel
    let mut pre_boot_queue = Vec::<Result<t::KernelMessage, t::WrappedSendError>>::new();
    while let Some(message) = recv_in_process.recv().await {
        match message {
            Err(_) => {
                pre_boot_queue.push(message);
                continue;
            }
            Ok(message) => {
                if (message.source
                    == t::Address {
                        node: metadata.our.node.clone(),
                        process: KERNEL_PROCESS_ID.clone(),
                    })
                    && (message.message
                        == t::Message::Request(t::Request {
                            inherit: false,
                            expects_response: None,
                            body: b"run".to_vec(),
                            metadata: None,
                            capabilities: vec![],
                        }))
                {
                    break;
                }
                pre_boot_queue.push(Ok(message));
            }
        }
    }
    // now that we've received the run message, we can send the pre-boot queue
    for message in pre_boot_queue {
        send_to_process.send(message).await?;
    }

    let component =
        Component::new(&engine, wasm_bytes.clone()).expect("make_process_loop: couldn't read file");

    let mut linker = Linker::new(&engine);
    Process::add_to_linker(&mut linker, |state: &mut ProcessWasi| state).unwrap();

    let table = Table::new();
    let wasi_stderr = MemoryOutputPipe::new(STACK_TRACE_SIZE);
    let wasi = WasiCtxBuilder::new().stderr(wasi_stderr.clone()).build();

    wasmtime_wasi::preview2::command::add_to_linker(&mut linker).unwrap();

    let mut store = Store::new(
        &engine,
        ProcessWasi {
            process: ProcessState {
                keypair: keypair.clone(),
                metadata: metadata.clone(),
                recv_in_process,
                self_sender: send_to_process,
                send_to_loop: send_to_loop.clone(),
                send_to_terminal: send_to_terminal.clone(),
                nested_request: None,
                current_incoming_message: None,
                contexts: HashMap::new(),
                message_queue: VecDeque::new(),
                caps_oracle: caps_oracle.clone(),
            },
            table,
            wasi,
        },
    );

    let (bindings, _bindings) =
        match Process::instantiate_async(&mut store, &component, &linker).await {
            Ok(b) => b,
            Err(e) => {
                let _ = send_to_terminal
                    .send(t::Printout {
                        verbosity: 0,
                        content: format!(
                            "mk: process {:?} failed to instantiate: {:?}",
                            metadata.our.process, e,
                        ),
                    })
                    .await;
                return Err(e);
            }
        };

    // the process will run until it returns from init() or crashes
    let is_error = match bindings
        .call_init(&mut store, &metadata.our.to_string())
        .await
    {
        Ok(()) => {
            let _ = send_to_terminal
                .send(t::Printout {
                    verbosity: 1,
                    content: format!("process {} returned without error", metadata.our.process),
                })
                .await;
            false
        }
        Err(_) => {
            let stderr = wasi_stderr.contents().into();
            let stderr = String::from_utf8(stderr)?;
            let _ = send_to_terminal
                .send(t::Printout {
                    verbosity: 0,
                    content: format!(
                        "\x1b[38;5;196mprocess {} ended with error:\x1b[0m\n{}",
                        metadata.our.process, stderr,
                    ),
                })
                .await;
            true
        }
    };

    //
    // the process has completed, time to perform cleanup
    //

    let our_kernel = t::Address {
        node: metadata.our.node.clone(),
        process: KERNEL_PROCESS_ID.clone(),
    };

    // get caps before killing
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = caps_oracle
        .send(t::CapMessage::GetAll {
            on: metadata.our.process.clone(),
            responder: tx,
        })
        .await;
    let initial_capabilities = rx
        .await?
        .iter()
        .map(|c| t::Capability {
            issuer: c.0.issuer.clone(),
            params: c.0.params.clone(),
        })
        .collect();

    // send message to tell main kernel loop to remove handler
    send_to_loop
        .send(t::KernelMessage {
            id: rand::random(),
            source: our_kernel.clone(),
            target: our_kernel.clone(),
            rsvp: None,
            message: t::Message::Request(t::Request {
                inherit: false,
                expects_response: None,
                body: serde_json::to_vec(&t::KernelCommand::KillProcess(
                    metadata.our.process.clone(),
                ))
                .unwrap(),
                metadata: None,
                capabilities: vec![],
            }),
            lazy_load_blob: None,
        })
        .await?;

    // fulfill the designated OnExit behavior
    match metadata.on_exit {
        t::OnExit::None => {
            let _ = send_to_terminal
                .send(t::Printout {
                    verbosity: 1,
                    content: format!("process {} had no OnExit behavior", metadata.our.process),
                })
                .await;
        }
        // if restart, tell ourselves to init the app again, with same capabilities
        t::OnExit::Restart => {
            if is_error {
                let _ = send_to_terminal
                    .send(t::Printout {
                        verbosity: 0,
                        content: format!(
                            "skipping OnExit::Restart for process {} due to crash",
                            metadata.our.process
                        ),
                    })
                    .await;
            } else {
                let _ = send_to_terminal
                    .send(t::Printout {
                        verbosity: 1,
                        content: format!(
                            "firing OnExit::Restart for process {}",
                            metadata.our.process
                        ),
                    })
                    .await;
                send_to_loop
                    .send(t::KernelMessage {
                        id: rand::random(),
                        source: our_kernel.clone(),
                        target: our_kernel.clone(),
                        rsvp: None,
                        message: t::Message::Request(t::Request {
                            inherit: false,
                            expects_response: None,
                            body: serde_json::to_vec(&t::KernelCommand::InitializeProcess {
                                id: metadata.our.process.clone(),
                                wasm_bytes_handle: metadata.wasm_bytes_handle,
                                wit_version: Some(metadata.wit_version),
                                on_exit: metadata.on_exit,
                                initial_capabilities,
                                public: metadata.public,
                            })
                            .unwrap(),
                            metadata: None,
                            capabilities: vec![],
                        }),
                        lazy_load_blob: Some(t::LazyLoadBlob {
                            mime: None,
                            bytes: wasm_bytes,
                        }),
                    })
                    .await?;
                send_to_loop
                    .send(t::KernelMessage {
                        id: rand::random(),
                        source: our_kernel.clone(),
                        target: our_kernel.clone(),
                        rsvp: None,
                        message: t::Message::Request(t::Request {
                            inherit: false,
                            expects_response: None,
                            body: serde_json::to_vec(&t::KernelCommand::RunProcess(
                                metadata.our.process.clone(),
                            ))
                            .unwrap(),
                            metadata: None,
                            capabilities: vec![],
                        }),
                        lazy_load_blob: None,
                    })
                    .await?;
            }
        }
        // if requests, fire them
        // even in death, a process can only message processes it has capabilities for
        t::OnExit::Requests(requests) => {
            send_to_terminal
                .send(t::Printout {
                    verbosity: 1,
                    content: format!(
                        "firing OnExit::Requests for process {}",
                        metadata.our.process
                    ),
                })
                .await?;
            for (address, mut request, blob) in requests {
                request.expects_response = None;
                let (tx, rx) = tokio::sync::oneshot::channel();
                caps_oracle
                    .send(t::CapMessage::Has {
                        on: metadata.our.process.clone(),
                        cap: t::Capability {
                            issuer: address.clone(),
                            params: "\"messaging\"".into(),
                        },
                        responder: tx,
                    })
                    .await?;
                if let Ok(true) = rx.await {
                    send_to_loop
                        .send(t::KernelMessage {
                            id: rand::random(),
                            source: metadata.our.clone(),
                            target: address,
                            rsvp: None,
                            message: t::Message::Request(request),
                            lazy_load_blob: blob,
                        })
                        .await?;
                }
            }
        }
    }
    Ok(())
}
