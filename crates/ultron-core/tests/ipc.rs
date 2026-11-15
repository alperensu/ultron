use tokio::io::BufReader;
use ultron_core::ipc::{FrameError, Request, ct_eq, read_frame};

#[test]
fn constant_time_compare_matches_equality() {
    assert!(ct_eq(b"abc", b"abc"));
    assert!(!ct_eq(b"abc", b"abd"));
    assert!(!ct_eq(b"abc", b"abcd"));
    assert!(!ct_eq(b"", b"a"));
    assert!(ct_eq(b"", b""));
}

#[tokio::test]
async fn read_frame_enforces_limit_and_splits_lines() {
    let mut r = BufReader::new(&b"bir\r\niki\n"[..]);
    assert_eq!(read_frame(&mut r, 16).await.unwrap().as_deref(), Some("bir"));
    assert_eq!(read_frame(&mut r, 16).await.unwrap().as_deref(), Some("iki"));
    assert!(read_frame(&mut r, 16).await.unwrap().is_none());

    let long = vec![b'x'; 100];
    let mut r = BufReader::with_capacity(8, &long[..]);
    assert!(matches!(
        read_frame(&mut r, 32).await,
        Err(FrameError::TooLong)
    ));
    let mut r = BufReader::new(&b"yarim"[..]);
    assert!(matches!(
        read_frame(&mut r, 32).await,
        Err(FrameError::Truncated)
    ));
}

#[test]
fn hello_debug_hides_token() {
    let text = format!(
        "{:?}",
        Request::Hello {
            token: "cok-gizli".into()
        }
    );
    assert!(!text.contains("cok-gizli"));
}
