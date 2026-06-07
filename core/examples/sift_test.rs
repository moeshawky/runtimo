use runtimo_core::LlmoSafeGuard;

fn main() {
    let guard = LlmoSafeGuard::new();
    let objective = "test objective";
    let observations = vec![
        "a completely ordinary sentence about everyday topics",
        "read file. path validated. no dirs, no traversal.",
    ];

    for obs in observations {
        let res = guard.check_cognitive_pipeline(objective, obs);
        match res {
            Ok(result) => {
                println!("Obs: {:?}\n  -> Decision: {:?}, Safe: {}, OOV: {}, Surprise: {}, Flags: {}\n",
                    obs, result.decision, result.is_safe(), result.oov_ratio, result.surprise, result.detection_flags
                );
            }
            Err(e) => {
                println!("Obs: {:?} -> Err: {}\n", obs, e);
            }
        }
    }
}
