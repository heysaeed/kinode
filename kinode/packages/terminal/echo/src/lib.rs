use kinode_process_lib::{script, Address};

wit_bindgen::generate!({
    path: "target/wit",
    world: "process-v0",
});

script!(init);
fn init(_our: Address, args: String) -> String {
    args
}
