/// Flags a possible microphone-to-speaker route using names alone.
///
/// Substring matching accommodates vendor prefixes and localized device names,
/// but short terms such as "mic" can match unrelated names. Audio interfaces may
/// hide both ends of a risky route. A false result therefore does not establish
/// safety; the UI uses a true result to require explicit connection confirmation.
pub fn feedback_risk(input: &str, output: &str) -> bool {
    let input = input.to_lowercase();
    let output = output.to_lowercase();
    let microphone = ["microphone", "mic", "マイク", "麦克风", "麥克風"]
        .iter()
        .any(|word| input.contains(word));
    let speaker = [
        "speaker",
        "スピーカー",
        "スピーカ",
        "扬声器",
        "揚聲器",
        "haut-parleur",
        "lautsprecher",
    ]
    .iter()
    .any(|word| output.contains(word));
    microphone && speaker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_for_named_microphones_and_speakers() {
        assert!(feedback_risk(
            "MacBook Pro Microphone",
            "MacBook Pro Speakers"
        ));
        assert!(feedback_risk("USB MIC", "External SPEAKERS"));
        assert!(feedback_risk("内蔵マイク", "内蔵スピーカー"));
    }

    #[test]
    fn does_not_flag_other_routes() {
        assert!(!feedback_risk("Microphone", "Headphones"));
        assert!(!feedback_risk("Line In", "Speakers"));
        assert!(!feedback_risk("USB Interface", "USB Interface"));
    }
}
