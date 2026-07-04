use super::*;

const NPUB: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const NYM: &str = "testnym";
const CODE: &str = "ABCDEFGH";
const LABEL: &str = "Front Counter";
const POS_DESCRIPTOR: &str = "ct(slip77(9c8e4f05c7711a98c838be228bcb84924d4570ca53f35fa1c793e58841d47023),elwpkh([73c5da0a/84h/1776h/0h]xpub6CRFzUgHFDaiDAQFNX7VeV9JNPDRabq6NYSpzVZ8zW8ANUCiDdenkb1gBoEZuXNZb3wPc1SVcDXgD2ww5UBtTb8s8ArAbTkoRQ8qn34KgcY/<0;1>/*))#y8jljyxl";
const TS: u64 = 1_750_000_000;

#[test]
fn pos_pair_message_byte_contract() {
    let msg = auth::build_la_v2_message(ACTION_PAIR, NPUB, NYM, &[CODE, LABEL, POS_DESCRIPTOR], TS);
    let expected_hex = "62756c6c7061792d6c612d763200706f732d70616972003031323334353637383961626364656630313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656600746573746e796d0041424344454647480046726f6e7420436f756e74657200637428736c697037372839633865346630356337373131613938633833386265323238626362383439323464343537306361353366333566613163373933653538383431643437303233292c656c77706b68285b37336335646130612f3834682f31373736682f30685d78707562364352467a55674846446169444151464e5837566556394a4e504452616271364e5953707a565a387a5738414e5543694464656e6b623167426f455a75584e5a6233775063315356634458674432777735554274546238733841724162546b6f525138716e33344b6763592f3c303b313e2f2a29292379386a6c6a79786c0031373530303030303030";
    assert_eq!(hex::encode(msg), expected_hex);
}

#[test]
fn pos_terminal_list_message_byte_contract() {
    let msg = auth::build_la_v2_message(ACTION_TERMINAL_LIST, NPUB, "", &[], TS);
    let expected_hex = "62756c6c7061792d6c612d763200706f732d7465726d696e616c2d6c6973740030313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566000031373530303030303030";
    assert_eq!(hex::encode(msg), expected_hex);
}

#[test]
fn pos_terminal_revoke_message_byte_contract() {
    let terminal_id = "550e8400-e29b-41d4-a716-446655440000";
    let msg = auth::build_la_v2_message(ACTION_TERMINAL_REVOKE, NPUB, "", &[terminal_id], TS);
    let expected_hex = "62756c6c7061792d6c612d763200706f732d7465726d696e616c2d7265766f6b650030313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566000035353065383430302d653239622d343164342d613731362d3434363635353434303030300031373530303030303030";
    assert_eq!(hex::encode(msg), expected_hex);
}

#[test]
fn token_hash_format_validation_cases() {
    assert!(is_hex64(&"a".repeat(64)));
    assert!(is_hex64(&"0123456789abcdef".repeat(4)));
    assert!(!is_hex64(&"A".repeat(64)));
    assert!(!is_hex64(&"g".repeat(64)));
    assert!(!is_hex64(&"a".repeat(63)));
    assert!(!is_hex64(&"a".repeat(65)));
}

#[test]
fn pairing_code_uses_unambiguous_alphabet() {
    for _ in 0..128 {
        let code = generate_pairing_code();
        assert_eq!(code.len(), 8);
        for ch in code.bytes() {
            assert!(PAIRING_CODE_ALPHABET.contains(&ch));
            assert!(!b"0O1I".contains(&ch));
        }
    }
}

#[test]
fn label_validation_rejects_control_chars_and_long_values() {
    assert_eq!(
        validate_label(Some("Front Counter")).unwrap().as_deref(),
        Some("Front Counter")
    );
    assert!(validate_label(Some("Front\nCounter")).is_err());
    assert!(validate_label(Some(&"a".repeat(101))).is_err());
}

#[test]
fn memo_validation_accepts_empty_and_normal_text() {
    assert_eq!(invoice::validate_pos_memo(None).unwrap(), None);
    assert_eq!(invoice::validate_pos_memo(Some("")).unwrap(), None);
    assert_eq!(
        invoice::validate_pos_memo(Some("Two coffees"))
            .unwrap()
            .as_deref(),
        Some("Two coffees")
    );
    assert!(invoice::validate_pos_memo(Some(&"a".repeat(280))).is_ok());
}

#[test]
fn memo_validation_rejects_long_and_control_chars() {
    assert!(invoice::validate_pos_memo(Some(&"a".repeat(281))).is_err());
    assert!(invoice::validate_pos_memo(Some("Line one\nLine two")).is_err());
    assert!(invoice::validate_pos_memo(Some("Tabbed\tmemo")).is_err());
}
