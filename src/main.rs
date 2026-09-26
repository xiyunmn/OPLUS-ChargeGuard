fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match charge_guard::runtime::execute(&args) {
        Ok(v) => println!("{}", v),
        Err(e) => {
            eprintln!("{}", serde_json::json!({"ok":false,"error":e}));
            std::process::exit(1);
        }
    }
}
