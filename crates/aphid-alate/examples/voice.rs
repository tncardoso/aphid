//! Read a recording aloud, with the real model.
//!
//! Not a test: it fetches 670 MB the first time and then does arithmetic for a
//! few seconds, which is not what `cargo test` is for. It is the way to see
//! the whole path work on one machine.
//!
//! ```console
//! $ cargo run --release --example voice --features voice -- recording.ogg
//! ```

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: voice <recording>");
        std::process::exit(2);
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    runtime.block_on(async {
        use aphid_alate::config::Voice as Configured;
        use aphid_alate::voice::{Files, Transcribe, Voice};

        let configured = Configured::default();
        let files = Files::new(configured.directory());
        if let Err(why) = files.fetch().await {
            eprintln!("the model: {why}");
            std::process::exit(1);
        }

        let bytes = std::fs::read(&path).expect("the recording");
        let started = std::time::Instant::now();
        let voice = Voice::new(files, None);
        match voice.transcribe(bytes).await {
            Ok(text) => println!("\n🎤 {text}\n\nread in {:.1?}", started.elapsed()),
            Err(why) => {
                eprintln!("{why}");
                std::process::exit(1);
            }
        }
    });
}
