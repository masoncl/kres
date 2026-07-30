//! Debug tool: feed a text file through the strict response contract and
//! print the accepted response or its validation errors.
//!
//! Run: `cargo run -p kres-agents --example parse_lens -- /tmp/lens17.raw`
use std::env;
use std::fs;

fn main() {
    let path = env::args().nth(1).expect("usage: parse_lens <file>");
    let text = fs::read_to_string(&path).expect("read");
    let r = match kres_agents::response::CodeResponseContract::default().validate(&text) {
        Ok(response) => response,
        Err(errors) => {
            eprintln!("invalid response:\n- {}", errors.join("\n- "));
            std::process::exit(1);
        }
    };
    println!("strategy: {:?}", r.strategy);
    println!("analysis: {} chars", r.analysis.len());
    println!("followups: {}", r.followups.len());
    for fu in &r.followups {
        println!(
            "  followup kind={} name={} reason={}",
            fu.kind,
            fu.name,
            fu.reason.chars().take(80).collect::<String>()
        );
    }
    println!("findings: {}", r.findings.len());
    for fi in &r.findings {
        println!(
            "  finding[{:?}] {}: {}",
            fi.severity,
            fi.id,
            fi.title.chars().take(80).collect::<String>()
        );
    }
    println!("skill_reads: {:?}", r.skill_reads);
    println!("ready_for_slow: {}", r.ready_for_slow);
    println!("\nanalysis preview:");
    println!("{}", r.analysis.chars().take(400).collect::<String>());
}
