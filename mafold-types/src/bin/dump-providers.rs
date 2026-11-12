//! Print the provider packs the publish pipeline signs and uploads.
//!
//! The registry is still AUTHORED here, in Rust, with types and tests — what
//! changed is only how it reaches clients. This binary is the seam: one command
//! turns the authored const into the exact bytes `publish-providers.yml` signs,
//! so nobody hand-maintains a JSON copy that could drift from the rows the
//! tests check.
//!
//! Every publish carries two packs under one version (`connections::PACK_V1`):
//! pack 1 for clients from before pack 2 — only the rows they can take whole —
//! and pack 2 with every row.
//!
//!   cargo run -p mafold-types --bin dump-providers               # every row
//!   cargo run -p mafold-types --bin dump-providers -- rows 1     # pack 1's rows
//!   cargo run -p mafold-types --bin dump-providers -- digest 2 42   # what to sign
//!
//! `digest` prints the SHA-256 as lowercase hex for a given pack and version,
//! because shell pipelines cannot compute it and signing anything else would
//! produce a pack that verifies nowhere.

use mafold_types::connections::{
    pack_digest, provider_infos, provider_rows, rows_checksum, v1_rows, ProviderInfo, PACK_V1, PACK_V2,
};

fn rows_of(pack: Option<&String>) -> Vec<ProviderInfo> {
    match pack.and_then(|p| p.parse::<u32>().ok()) {
        Some(PACK_V1) => v1_rows(&provider_infos()),
        Some(PACK_V2) => provider_infos(),
        _ => {
            eprintln!("which pack? 1 or 2");
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("rows") => println!(
            "{}",
            serde_json::to_string(&provider_rows(&rows_of(args.get(1)))).expect("the registry serialises")
        ),
        Some("digest") => {
            let rows = provider_rows(&rows_of(args.get(1)));
            let pack: u32 = args[1].parse().expect("checked by rows_of");
            let version: u32 = args
                .get(2)
                .and_then(|v| v.parse().ok())
                .expect("usage: dump-providers digest <pack> <version>");
            let d = pack_digest(pack, version, &rows);
            println!("{}", d.iter().map(|b| format!("{b:02x}")).collect::<String>());
        }
        Some("checksum") => println!("{}", rows_checksum(&provider_rows(&rows_of(args.get(1))))),
        Some("count") => println!("{}", rows_of(args.get(1)).len()),
        None => println!(
            "{}",
            serde_json::to_string(&provider_infos()).expect("the registry serialises")
        ),
        Some(other) => {
            eprintln!("unknown subcommand `{other}` — try rows/digest/checksum/count <pack>, or none");
            std::process::exit(2);
        }
    }
}
