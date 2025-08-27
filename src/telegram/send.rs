use std::fmt::Display;

pub fn send_lead_manager_message<T: Display>(message: &T) {
    println!("Sending lead manager message: {message}");
}
