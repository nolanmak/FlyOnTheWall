use fotw_stt::local::{EndpointConfig, Endpointer};

#[test]
fn silence_and_a_steady_background_do_not_accumulate_a_speech_stall() {
    for level in [0, 500, 8_000, 20_000] {
        let mut vad = Endpointer::new(EndpointConfig::default());
        for _ in 0..70 {
            let _ = vad.push(&vec![level; 16_000]);
        }
        assert_eq!(
            vad.voiced_samples(),
            0,
            "steady background at {level} counted as speech"
        );
    }
}

#[test]
fn variable_speech_advances_the_counter_and_later_silence_does_not() {
    let mut vad = Endpointer::new(EndpointConfig::default());
    for frame in 0..100 {
        let _ = vad.push(&vec![if frame % 2 == 0 { 1_000 } else { 8_000 }; 480]);
    }
    let speech = vad.voiced_samples();
    assert!(speech >= 2 * 16_000);
    for _ in 0..10 {
        let _ = vad.push(&vec![0; 16_000]);
    }
    assert_eq!(vad.voiced_samples(), speech);
}
