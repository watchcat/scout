//! Reads a shop's delivery terms the way the delivery desk would, with the
//! side model the environment configures, and prints as one line of JSON
//! what was found, what the model answered, what Rust kept of it and which
//! check refused anything. For trying the desk's prompt on a real page and
//! a real model. It stores nothing.
//!
//!     delivery_as_read <a page of the shop> <country> [--save text.txt]
//!
//! The page is any page of the shop that links its delivery page: a
//! product page, as an offer's address would be. The country is a code or
//! a name, `NL` or `Netherlands`.
//!
//! The model is the one side calls go to: `SIDE_LLM_MODEL`, with
//! `SIDE_LLM_BASE_URL` and `SIDE_LLM_API_KEY` where they are set, and
//! otherwise `LLM_MODEL`, `LLM_BASE_URL` and `LLM_API_KEY` (or the
//! `MINIMAX_` names). They are read from the environment and not from
//! `.env`, which whoever runs this exports first. Neither the key nor any
//! header is printed.
//!
//! `--save` writes the text the model was handed, which every amount in
//! its answer is checked against; the line itself says only how long that
//! text was.
//!
//! It does not go through `net_guard`: the address comes from whoever runs
//! it, not from a model.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = match (args.first(), args.get(1)) {
        (Some(page), Some(country)) if !page.starts_with("--") && !country.starts_with("--") => {
            read(page, country, flag(&args, "--save"))
        }
        _ => {
            eprintln!("usage: delivery_as_read <a page of the shop> <country> [--save <file for the text the model was handed>]");
            std::process::exit(2);
        }
    };
    println!("{out}");
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn read(page: &str, country: &str, save: Option<&str>) -> serde_json::Value {
    let side = match scout_core::config::SideLlm::from_env() {
        Ok(side) => side,
        Err(e) => return serde_json::json!({"error": e.to_string()}),
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    let mut out = match runtime.block_on(scout_core::measure::delivery_as_read(page, country, &side)) {
        Ok(out) => out,
        Err(e) => return serde_json::json!({"error": e.to_string()}),
    };
    // Up to 12,000 characters of a page have no place on the line.
    let text = out.as_object_mut().and_then(|line| line.remove("text"));
    if let (Some(path), Some(text)) = (save, text.as_ref().and_then(|text| text.as_str())) {
        let _ = std::fs::write(path, text);
    }
    out
}
