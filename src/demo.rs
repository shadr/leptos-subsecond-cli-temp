use dioxus_devtools::DevserverMsg;

fn foo() {
    let a = 10;
    let b = 123123;
    dbg!(a + b);
}

fn main() {
    dioxus_devtools::connect_at("ws://127.0.0.1:3100/".to_string(), |msg| {
        if let DevserverMsg::HotReload(hot_reload_msg) = msg {
            if let Some(jumptable) = hot_reload_msg.jump_table {
                dbg!(hot_reload_msg.for_pid, std::process::id());
                if hot_reload_msg.for_pid == Some(std::process::id()) {
                    unsafe { subsecond::apply_patch(jumptable).unwrap() };
                }
            }
        }
    });
    loop {
        subsecond::call(|| foo());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
