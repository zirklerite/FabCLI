use serde_json::Value;

pub fn print_json(value: &Value, pretty: bool) {
    let s = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("serializing a constructed json::Value cannot fail");
    println!("{}", s);
}
