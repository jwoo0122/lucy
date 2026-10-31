use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LUCY_AGENT_DESKTOP_ARCHIVE");
    let target = env::var("TARGET").expect("Cargo target");
    let digest = match target.as_str() {
        "aarch64-apple-darwin" => {
            "7bee385196d7b478de80cf05ca4304aaa38f0eba593b3406ad3c247c1773be19"
        }
        "x86_64-apple-darwin" => "8edda6df17eafc042395502a823dbb8415bc8ac14835a6731e4d8becc9cb4121",
        "x86_64-unknown-linux-gnu" => {
            "ad661b7a3fd8f990a769243f4889a240ca0e2c8dab3ef5bddd6019086c71e0b4"
        }
        _ => panic!("agent-desktop is not bundled for target {target}"),
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
        .join("agent-desktop.tar.gz");
    let bytes = if let Some(path) = env::var_os("LUCY_AGENT_DESKTOP_ARCHIVE") {
        println!("cargo:rerun-if-changed={}", PathBuf::from(&path).display());
        fs::read(path).expect("read offline agent-desktop archive")
    } else if output.exists() {
        fs::read(&output).expect("read cached agent-desktop archive")
    } else {
        let url = format!("https://github.com/vercel-labs/agent-desktop/releases/download/v1.0.2/agent-desktop-v1.0.2-{target}.tar.gz");
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("download client")
            .get(url)
            .send()
            .expect("download agent-desktop; for offline builds set LUCY_AGENT_DESKTOP_ARCHIVE")
            .error_for_status()
            .expect("agent-desktop download status")
            .bytes()
            .expect("agent-desktop archive bytes")
            .to_vec()
    };
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        digest,
        "agent-desktop SHA-256 mismatch"
    );
    fs::write(output, bytes).expect("write verified agent-desktop archive");
    println!("cargo:rustc-env=LUCY_DESKTOP_TARGET={target}");
}
