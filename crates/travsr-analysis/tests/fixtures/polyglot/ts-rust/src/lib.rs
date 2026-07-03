#[napi]
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[napi(js_name = "greet")]
pub fn greet_impl(name: String) -> String {
    format!("Hello, {name}!")
}
