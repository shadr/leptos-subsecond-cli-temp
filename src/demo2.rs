fn foo() {
    println!("4");
}

fn main() {
    dioxus_devtools::connect_subsecond();
    loop {
        subsecond::call(|| foo());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
