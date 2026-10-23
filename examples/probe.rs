//! Run one jq program through the sandbox: `cargo run --example probe -- '<program>' '<json>'`.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(2).cloned().unwrap_or_else(|| "null".into());
    match ikigai_json::sandbox::run(&args[1], input.as_bytes(), &Default::default()) {
        Ok(out) => out.iter().for_each(|o| println!("{}", o.json)),
        Err(e) => println!("REFUSED {e}"),
    }
}
