use std::{
    net::TcpListener,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use dioxus_devtools::DevserverMsg;
use tungstenite::handshake::server::{Request, Response};

pub struct HotPatchServer {
    addr: String,
    patch_receiver: multiqueue::BroadcastReceiver<DevserverMsg>,
    aslr_reference: Arc<AtomicU64>,
}

impl HotPatchServer {
    pub fn new(
        addr: &str,
        patch_receiver: multiqueue::BroadcastReceiver<DevserverMsg>,
        aslr_reference: Arc<AtomicU64>,
    ) -> Self {
        Self {
            addr: addr.to_string(),
            patch_receiver,
            aslr_reference,
        }
    }

    pub fn run(&self) {
        let server = TcpListener::bind(&self.addr).unwrap();
        // TODO?: send accumulated patches to newly connected clients
        // we don't modify original binary so if you run binary a second time then it wont have
        // patches that been made between first and second launches
        //
        // UPD: now it uses `multiqueue` crate which provides broadcast spmc/mpmc channels
        // and using `add_stream` seems to achieve that behaviour, sending previously built patches
        // TODO: if we need to do a fat rebuild, then we need to clean previous patches
        for new_stream in server.incoming() {
            if let Ok(stream) = new_stream {
                let channel = self.patch_receiver.add_stream();
                let aslr_reference = Arc::clone(&self.aslr_reference);
                std::thread::spawn(move || {
                    let mut websocket =
                        tungstenite::accept_hdr(stream, |request: &Request, response: Response| {
                            if let Some(query) = request.uri().query() {
                                let split = query.split("&");
                                // a little bit ugly hack to get aslr back to the builder
                                // TODO: find another way to get aslr reference back
                                for s in split {
                                    if let Some(aslr_str) = s.strip_prefix("aslr_reference=") {
                                        if let Ok(new_aslr_reference) = aslr_str.parse() {
                                            if new_aslr_reference != 0 {
                                                aslr_reference.store(
                                                    new_aslr_reference,
                                                    std::sync::atomic::Ordering::SeqCst,
                                                );
                                            }
                                        }
                                        break;
                                    }
                                }
                            }
                            Ok(Response::from(response))
                        })
                        .unwrap();
                    println!("WS connected");

                    loop {
                        if !websocket.can_write() {
                            break;
                        }
                        if let Ok(msg) = channel.try_recv() {
                            let serialized = serde_json::to_string(&msg).unwrap();
                            websocket
                                .send(tungstenite::Message::Text(serialized.into()))
                                .unwrap();
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    println!("WS loop exited");
                });
            }
        }
    }
}
