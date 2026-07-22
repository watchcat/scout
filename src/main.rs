// Modules are added task by task; dead_code is allowed until final wiring
// (removed in the last task).
#![allow(dead_code)]

mod agent;
mod config;
mod draft;
mod scheduler;
mod store;
mod text;
mod tools;
mod vision;

fn main() {
    println!("scout: not wired up yet");
}
