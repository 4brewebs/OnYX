use onyx_core::DetRng;

#[test]
fn fixed_vectors_match_the_agreed_integer_algorithm() {
    // Vectores calculados con una implementación Python de enteros módulo 2^64.
    // No son una certificación estadística del generador.
    let vectors: [(u64, u64, [u64; 4]); 4] = [
        (
            0,
            0,
            [
                10681102536704768537,
                3816651431128748797,
                3768063224490501920,
                13993169304313749814,
            ],
        ),
        (
            1,
            2,
            [
                3999644361938179224,
                2243154290913709655,
                9006122794859499487,
                1096643349416250318,
            ],
        ),
        (
            2,
            1,
            [
                8226119660689351819,
                365986473265698315,
                17499228419649977782,
                13794788728372014692,
            ],
        ),
        (
            18446744073709551615,
            18446744073709551615,
            [
                5031991619706209320,
                6275280195239067813,
                5151237670279393552,
                525489437549836252,
            ],
        ),
    ];
    for (seed, tick, expected) in vectors {
        let mut rng = DetRng::from_seed_and_tick(seed, tick);
        for value in expected {
            assert_eq!(rng.next_u64(), value);
        }
    }
}

#[test]
fn equal_inputs_and_calls_replay_the_same_sequence() {
    let mut a = DetRng::from_seed_and_tick(91, 73);
    let mut b = DetRng::from_seed_and_tick(91, 73);
    for _ in 0..1024 {
        assert_eq!(a.next_u64(), b.next_u64());
        assert_eq!(a.next_f64().to_bits(), b.next_f64().to_bits());
        assert_eq!(a.gen_range(10, 17), b.gen_range(10, 17));
    }
}

#[test]
fn zero_initial_mix_does_not_trap_the_generator() {
    let mut rng = DetRng::from_seed_and_tick(0, 477719569315861442);
    let expected = [
        9018836209304764660,
        2737245654252273974,
        8048510555075670271,
        5976771191580929590,
    ];
    for value in expected {
        assert_ne!(value, 0);
        assert_eq!(rng.next_u64(), value);
    }
}

#[test]
fn invalid_intervals_return_low_without_advancing_the_sequence() {
    let mut rng = DetRng::from_seed_and_tick(1, 2);
    let mut untouched = rng.clone();
    assert_eq!(rng.gen_range(5, 5), 5);
    assert_eq!(rng.gen_range(9, 3), 9);
    assert_eq!(rng.next_u64(), untouched.next_u64());
}

#[test]
fn floats_and_integer_ranges_stay_within_their_bounds() {
    let mut rng = DetRng::from_seed_and_tick(u64::MAX, 0);
    for _ in 0..4096 {
        assert!((0.0..1.0).contains(&rng.next_f64()));
        for (low, high) in [
            (0, 1),
            (10, 17),
            (0, u64::MAX),
            (0, (1u64 << 63) + 1),
            (u64::MAX - 1, u64::MAX),
        ] {
            let value = rng.gen_range(low, high);
            assert!(low <= value && value < high);
        }
    }
}
