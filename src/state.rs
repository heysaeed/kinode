use anyhow::Result;
use rocksdb::checkpoint::Checkpoint;
use rocksdb::{Options, DB};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;

use crate::types::*;

pub async fn load_state(
    our_name: String,
    home_directory_path: String,
    runtime_extensions: Vec<(ProcessId, MessageSender, bool)>,
) -> Result<(ProcessMap, DB), StateError> {
    let state_path = format!("{}/kernel", &home_directory_path);

    if let Err(e) = fs::create_dir_all(&state_path).await {
        panic!("failed creating kernel state dir! {:?}", e);
    }

    // more granular kernel_state in column families

    // let mut options = Option::default().unwrap();
    // options.create_if_missing(true);
    //let db = DB::open_default(&state_directory_path_str).unwrap();
    let mut opts = Options::default();
    opts.create_if_missing(true);
    // let cf_name = "kernel_state";
    // let cf_descriptor = ColumnFamilyDescriptor::new(cf_name, Options::default());
    let db = DB::open_default(state_path).unwrap();
    let mut process_map: ProcessMap = HashMap::new();

    let kernel_id = KERNEL_PROCESS_ID.to_hash();
    match db.get(kernel_id) {
        Ok(Some(value)) => {
            process_map = bincode::deserialize::<ProcessMap>(&value).unwrap();
        }
        Ok(None) => {
            bootstrap(
                &our_name,
                home_directory_path.clone(),
                runtime_extensions.clone(),
                &mut process_map,
            ).await.unwrap();

            db.put(kernel_id, bincode::serialize(&process_map).unwrap())
                .unwrap();
        }
        Err(e) => {
            panic!("failed to load kernel state from db: {:?}", e);
        }
    }
    
    Ok((process_map, db))
}

pub async fn state_sender(
    our_name: String,
    send_to_loop: MessageSender,
    send_to_terminal: PrintSender,
    mut recv_state: MessageReceiver,
    db: DB,
    home_directory_path: String,
) -> Result<(), anyhow::Error> {
    let db = Arc::new(db);
    //  into main loop

    loop {
        tokio::select! {
            Some(km) = recv_state.recv() => {
                if our_name != km.source.node {
                    println!(
                        "fs: request must come from our_name={}, got: {}",
                        our_name, &km,
                    );
                    continue;
                }
                let db_clone = db.clone();
                let send_to_loop = send_to_loop.clone();
                let send_to_terminal = send_to_terminal.clone();
                let our_name = our_name.clone();
                let home_directory_path = home_directory_path.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_request(
                            our_name.clone(),
                            km.clone(),
                            db_clone,
                            send_to_loop.clone(),
                            send_to_terminal,
                            home_directory_path,
                        )
                        .await
                        {
                            let _ = send_to_loop
                                .send(make_error_message(our_name.clone(), &km, e))
                                .await;
                        }
                });
            }
        }
    }
}

async fn handle_request(
    our_name: String,
    kernel_message: KernelMessage,
    db: Arc<DB>,
    send_to_loop: MessageSender,
    _send_to_terminal: PrintSender,
    home_directory_path: String,
) -> Result<(), StateError> {
    let KernelMessage {
        id,
        source,
        rsvp,
        message,
        payload,
        ..
    } = kernel_message;
    let Message::Request(Request {
        expects_response,
        ipc,
        metadata, // for kernel
        ..
    }) = message
    else {
        return Err(StateError::BadRequest {
            error: "not a request".into(),
        });
    };

    let action: StateAction = match serde_json::from_slice(&ipc) {
        Ok(r) => r,
        Err(e) => {
            return Err(StateError::BadJson {
                error: format!("parse into StateAction failed: {:?}", e),
            })
        }
    };

    let (ipc, bytes) = match action {
        StateAction::SetState(process_id) => {
            let key = process_id.to_hash();
            // TODO consistency with to_stirngs
            let Some(ref payload) = payload else {
                return Err(StateError::BadBytes {
                    action: "SetState".into(),
                });
            };

            db.put(key, &payload.bytes)?;
            (serde_json::to_vec(&StateResponse::SetState).unwrap(), None)
        }
        StateAction::GetState(process_id) => {
            let key = process_id.to_hash();
            match db.get(key) {
                Ok(Some(value)) => {
                    (
                        serde_json::to_vec(&StateResponse::GetState).unwrap(),
                        Some(value),
                    )
                }
                Ok(None) => {
                    return Err(StateError::NotFound {
                        process_id: process_id.clone(),
                    });
                }
                Err(e) => {
                    println!("get state error: {:?}", e);
                    return Err(StateError::RocksDBError {
                        action: "GetState".into(),
                        error: e.to_string(),
                    });
                }
            }
        }
        StateAction::DeleteState(process_id) => {
            // handle DeleteState action
            println!("got deleteState");
            let key = process_id.to_hash();
            match db.delete(key) {
                Ok(_) => {
                    println!("delete state success");
                    (
                        serde_json::to_vec(&StateResponse::DeleteState).unwrap(),
                        None,
                    )
                }
                Err(e) => {
                    println!("delete state error: {:?}", e);
                    return Err(StateError::RocksDBError {
                        action: "DeleteState".into(),
                        error: e.to_string(),
                    });
                }
            }
        }
        StateAction::Backup => {
            // handle Backup action
            println!("got backup");
            let checkpoint_dir = format!("{}/kernel/checkpoint", &home_directory_path);

            if Path::new(&checkpoint_dir).exists() {
                let _ = fs::remove_dir_all(&checkpoint_dir).await;
            }
            let checkpoint = Checkpoint::new(&db).unwrap();
            checkpoint.create_checkpoint(&checkpoint_dir).unwrap();
            (serde_json::to_vec(&StateResponse::Backup).unwrap(), None)
        }
    };

    if let Some(target) = rsvp.or_else(|| {
        expects_response.map(|_| Address {
            node: our_name.clone(),
            process: source.process.clone(),
        })
    }) {
        let response = KernelMessage {
            id,
            source: Address {
                node: our_name.clone(),
                process: STATE_PROCESS_ID.clone(),
            },
            target,
            rsvp: None,
            message: Message::Response((
                Response {
                    inherit: false,
                    ipc,
                    metadata,
                },
                None,
            )),
            payload: bytes.map(|bytes| Payload {
                mime: Some("application/octet-stream".into()),
                bytes,
            }),
            signed_capabilities: None,
        };

        let _ = send_to_loop.send(response).await;
    };

    Ok(())
}

/// function run only upon fresh boot.
///
/// for each folder in /modules, looks for a package.zip file, extracts the contents,
/// sends the contents to VFS, and reads the manifest.json.
///
/// the manifest.json contains instructions for which processes to boot and what
/// capabilities to give them. since we are inside runtime, can spawn those out of
/// thin air.
async fn bootstrap(
    our_name: &str,
    home_directory_path: String,
    runtime_extensions: Vec<(ProcessId, MessageSender, bool)>,
    process_map: &mut ProcessMap,
) -> Result<()> {
    println!("bootstrapping node...\r");

    let mut runtime_caps: HashSet<Capability> = HashSet::new();
    // kernel is a special case
    runtime_caps.insert(Capability {
        issuer: Address {
            node: our_name.to_string(),
            process: ProcessId::from_str("kernel:sys:uqbar").unwrap(),
        },
        params: "\"messaging\"".into(),
    });
    // net is a special case
    runtime_caps.insert(Capability {
        issuer: Address {
            node: our_name.to_string(),
            process: ProcessId::from_str("net:sys:uqbar").unwrap(),
        },
        params: "\"messaging\"".into(),
    });
    for runtime_module in runtime_extensions.clone() {
        runtime_caps.insert(Capability {
            issuer: Address {
                node: our_name.to_string(),
                process: runtime_module.0,
            },
            params: "\"messaging\"".into(),
        });
    }
    // give all runtime processes the ability to send messages across the network
    runtime_caps.insert(Capability {
        issuer: Address {
            node: our_name.to_string(),
            process: KERNEL_PROCESS_ID.clone(),
        },
        params: "\"network\"".into(),
    });

    // finally, save runtime modules in state map as well, somewhat fakely
    // special cases for kernel and net
    process_map
        .entry(ProcessId::from_str("kernel:sys:uqbar").unwrap())
        .or_insert(PersistedProcess {
            wasm_bytes_handle: "".into(),
            on_panic: OnPanic::Restart,
            capabilities: runtime_caps.clone(),
            public: false,
        });
    process_map
        .entry(ProcessId::from_str("net:sys:uqbar").unwrap())
        .or_insert(PersistedProcess {
            wasm_bytes_handle: "".into(),
            on_panic: OnPanic::Restart,
            capabilities: runtime_caps.clone(),
            public: false,
        });
    for runtime_module in runtime_extensions {
        process_map
            .entry(runtime_module.0)
            .or_insert(PersistedProcess {
                wasm_bytes_handle: "".into(),
                on_panic: OnPanic::Restart,
                capabilities: runtime_caps.clone(),
                public: runtime_module.2,
            });
    }

    let distro_path = format!("{}/vfs/kernel:sys:uqbar/", &home_directory_path);
    fs::create_dir_all(&distro_path).await.expect("bootstrap vfs dir creation failed!");

    let packages: Vec<(String, zip::ZipArchive<std::io::Cursor<Vec<u8>>>)> =
        get_zipped_packages().await;

    for (package_name, mut package) in packages {
        // special case tester: only load it in if in simulation mode
        if package_name == "tester" {
            #[cfg(not(feature = "simulation-mode"))]
            continue;
            #[cfg(feature = "simulation-mode")]
            {}
        }

        println!("fs: handling package {package_name}...\r");
        // get and read metadata.json
        let Ok(mut package_metadata_zip) = package.by_name("metadata.json") else {
            println!(
                "fs: missing metadata for package {}, skipping",
                package_name
            );
            continue;
        };
        let mut metadata_content = Vec::new();
        package_metadata_zip
            .read_to_end(&mut metadata_content)
            .unwrap();
        drop(package_metadata_zip);
        let package_metadata: serde_json::Value =
            serde_json::from_slice(&metadata_content).expect("fs: metadata parse error");

        println!("fs: found package metadata: {:?}\r", package_metadata);

        let package_name = package_metadata["package"]
            .as_str()
            .expect("fs: metadata parse error: bad package name");

        let package_publisher = package_metadata["publisher"]
            .as_str()
            .expect("fs: metadata parse error: bad publisher name");

        // create a new package in VFS
        let our_drive_name = [package_name, package_publisher].join(":");
        let drive_path = format!("/kernel:sys:uqbar/{}", &our_drive_name);

        let full_drive_path = format!("{}/{}", &distro_path, &our_drive_name);
        fs::create_dir(&full_drive_path).await.expect("vfs dir creation failed!");


        // for each file in package.zip, recursively through all dirs, send a newfile KM to VFS
        for i in 0..package.len() {
            let mut file = package.by_index(i).unwrap();
            if file.is_file() {
                let file_path = file
                    .enclosed_name()
                    .expect("fs: name error reading package.zip")
                    .to_owned();
                let mut file_path = file_path.to_string_lossy().to_string();
                if !file_path.starts_with('/') {
                    file_path = format!("/{}", file_path);
                }
                println!("fs: found file {}...\r", file_path);
                let mut file_content = Vec::new();
                file.read_to_end(&mut file_content).unwrap();
                let path = format!("{}/{}", &full_drive_path, file_path);
                fs::write(&path, file_content).await.unwrap();
            }
        }

        // get and read manifest.json
        let Ok(mut package_manifest_zip) = package.by_name("manifest.json") else {
            println!(
                "fs: missing manifest for package {}, skipping",
                package_name
            );
            continue;
        };
        let mut manifest_content = Vec::new();
        package_manifest_zip
            .read_to_end(&mut manifest_content)
            .unwrap();
        drop(package_manifest_zip);
        let package_manifest = String::from_utf8(manifest_content)?;
        let package_manifest = serde_json::from_str::<Vec<PackageManifestEntry>>(&package_manifest)
            .expect("fs: manifest parse error");

        // for each process-entry in manifest.json:
        for mut entry in package_manifest {
            let wasm_bytes = &mut Vec::new();
            let mut file_path = entry.process_wasm_path.to_string();
            if file_path.starts_with('/') {
                file_path = file_path[1..].to_string();
            }
            package
                .by_name(&file_path)
                .expect("fs: no wasm found in package!")
                .read_to_end(wasm_bytes)
                .unwrap();

            // spawn the requested capabilities
            // remember: out of thin air, because this is the root distro
            let mut requested_caps = HashSet::new();
            let our_process_id = format!(
                "{}:{}:{}",
                entry.process_name, package_name, package_publisher
            );
            entry.request_messaging = Some(entry.request_messaging.unwrap_or_default());
            if let Some(ref mut request_messaging) = entry.request_messaging {
                request_messaging.push(our_process_id.clone());
                for process_name in request_messaging {
                    requested_caps.insert(Capability {
                        issuer: Address {
                            node: our_name.to_string(),
                            process: ProcessId::from_str(process_name).unwrap(),
                        },
                        params: "\"messaging\"".into(),
                    });
                }
            }

            if entry.request_networking {
                requested_caps.insert(Capability {
                    issuer: Address {
                        node: our_name.to_string(),
                        process: KERNEL_PROCESS_ID.clone(),
                    },
                    params: "\"network\"".into(),
                });
            }

            // give access to package_name vfs
            requested_caps.insert(Capability {
                issuer: Address {
                    node: our_name.into(),
                    process: VFS_PROCESS_ID.clone(),
                },
                params: serde_json::to_string(&serde_json::json!({
                    "kind": "read",
                    "drive": drive_path,
                }))
                .unwrap(),
            });
            requested_caps.insert(Capability {
                issuer: Address {
                    node: our_name.into(),
                    process: VFS_PROCESS_ID.clone(),
                },
                params: serde_json::to_string(&serde_json::json!({
                    "kind": "write",
                    "drive": drive_path,
                }))
                .unwrap(),
            });

            let public_process = entry.public;

            let wasm_bytes_handle = format!(
                "{}/{}",
                &drive_path, &file_path
            );

            process_map.insert(
                ProcessId::new(Some(&entry.process_name), package_name, package_publisher),
                PersistedProcess {
                    wasm_bytes_handle,
                    on_panic: entry.on_panic,
                    capabilities: requested_caps,
                    public: public_process,
                },
            );
        }
    }
    Ok(())
}

/// go into /target folder and get all .zip package files
async fn get_zipped_packages() -> Vec<(String, zip::ZipArchive<std::io::Cursor<Vec<u8>>>)> {
    println!("fs: reading distro packages...\r");
    let target_path = std::path::Path::new("target");

    let mut packages = Vec::new();

    if let Ok(mut entries) = fs::read_dir(target_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().ends_with(".zip") {
                let package_name = entry
                    .file_name()
                    .to_string_lossy()
                    .trim_end_matches(".zip")
                    .to_string();
                if let Ok(bytes) = fs::read(entry.path()).await {
                    if let Ok(zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
                        // add to list of packages
                        println!("fs: found package: {}\r", package_name);
                        packages.push((package_name, zip));
                    }
                }
            }
        }
    }

    packages
}

fn make_error_message(our_name: String, km: &KernelMessage, error: StateError) -> KernelMessage {
    KernelMessage {
        id: km.id,
        source: Address {
            node: our_name.clone(),
            process: STATE_PROCESS_ID.clone(),
        },
        target: match &km.rsvp {
            None => km.source.clone(),
            Some(rsvp) => rsvp.clone(),
        },
        rsvp: None,
        message: Message::Response((
            Response {
                inherit: false,
                ipc: serde_json::to_vec(&StateResponse::Err(error)).unwrap(),
                metadata: None,
            },
            None,
        )),
        payload: None,
        signed_capabilities: None,
    }
}

impl From<std::io::Error> for VfsError {
    fn from(err: std::io::Error) -> Self {
        VfsError::IOError {
            error: err.to_string(),
            path: "".to_string(),
        } // replace with appropriate VfsError variant and fields
    }
}
impl From<rocksdb::Error> for StateError {
    fn from(error: rocksdb::Error) -> Self {
        StateError::RocksDBError {
            action: "ass".into(),
            error: error.to_string(),
        }
    }
}
