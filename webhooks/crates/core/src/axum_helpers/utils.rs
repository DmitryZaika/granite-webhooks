use std::env;

pub fn get_remix_key() -> String {
    env::var("REMIX_KEY").unwrap()
}
