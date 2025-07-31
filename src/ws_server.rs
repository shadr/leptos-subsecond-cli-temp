// This is very badly written websocket server for sending patches to connected clients.
// Do not use any following code anywhere else it's that bad. You will regret eventually if you decide to use it.

use std::{
    net::{TcpListener, TcpStream},
    sync::{Arc, atomic::AtomicU64, mpsc::Receiver},
    time::Duration,
};

use dioxus_devtools::DevserverMsg;
use multiqueue::BroadcastReceiver;
use tungstenite::handshake::server::{Request, Response};

pub struct HotPatchServer {
    addr: String,
    patch_receiver: BroadcastReceiver<DevserverMsg>,
    aslr_reference: Arc<AtomicU64>,
    clear_patches_rx: Receiver<()>,
}

impl HotPatchServer {
    pub fn new(
        addr: &str,
        patch_receiver: BroadcastReceiver<DevserverMsg>,
        aslr_reference: Arc<AtomicU64>,
        clear_patches_rx: Receiver<()>,
    ) -> Self {
        Self {
            addr: addr.to_string(),
            patch_receiver,
            aslr_reference,
            clear_patches_rx,
        }
    }

    pub fn run(&mut self) {
        let server = TcpListener::bind(&self.addr).unwrap();
        // TODO?: send accumulated patches to newly connected clients
        // we don't modify original binary so if you run binary a second time then it wont have
        // patches that been made between first and second launches
        //
        // UPD: now it uses `multiqueue` crate which provides broadcast spmc/mpmc channels
        // and using `add_stream` seems to achieve that behaviour, sending previously built patches
        // TODO: if we need to do a fat rebuild, then we need to clean previous patches
        //
        // these two above todos somewhat done, but I can't be sure with how badly this code looks
        for new_stream in server.incoming() {
            self.clear_patches_if_command_received();
            if let Ok(stream) = new_stream {
                let channel = self.patch_receiver.add_stream();
                let aslr_reference = Arc::clone(&self.aslr_reference);
                std::thread::spawn(move || Self::client_loop(stream, aslr_reference, channel));
            }
        }
    }

    pub fn clear_patches_if_command_received(&mut self) {
        if let Ok(_) = self.clear_patches_rx.try_recv() {
            while let Ok(_) = self.patch_receiver.try_recv() {}
        }
    }

    pub fn client_loop(
        stream: TcpStream,
        aslr_reference: Arc<AtomicU64>,
        patch_channel: BroadcastReceiver<DevserverMsg>,
    ) {
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
        tracing::debug!("New hot-patch client connected");

        loop {
            // this check I think do nothing to prevent trying to write a closed socket
            // I got a panic once in a thread, but currently don't care because thats not a main thread
            if !websocket.can_write() {
                break;
            }
            if let Ok(msg) = patch_channel.try_recv() {
                let serialized = serde_json::to_string(&msg).unwrap();
                websocket
                    .send(tungstenite::Message::Text(serialized.into()))
                    .unwrap();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
