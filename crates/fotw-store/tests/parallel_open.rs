//! A cold test process starts encrypted libraries concurrently, without real keys.
use fotw_store::{Db, DbKey};
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_cold_opens_key_independent_file_and_memory_libraries() {
    let root = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(32));
    let workers: Vec<_> = (0u8..32)
        .map(|id| {
            let path = root.path().join(format!("fixture-{id}.sqlite3"));
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let key = DbKey::from_bytes([id + 1; 32]);
                barrier.wait();
                let mut db = if id % 2 == 0 {
                    Db::open(&path, &key).unwrap()
                } else {
                    Db::open_in_memory(&key).unwrap()
                };
                db.put_setting("fixture", &id.to_string()).unwrap();
                assert_eq!(db.get_setting("fixture").unwrap(), Some(id.to_string()));
                assert!(db.schema_version().unwrap() > 0);
                if id % 2 == 0 {
                    drop(db);
                    let reopened = Db::open(&path, &key).unwrap();
                    assert_eq!(
                        reopened.get_setting("fixture").unwrap(),
                        Some(id.to_string())
                    );
                    drop(reopened);
                    let wrong = DbKey::from_bytes([id + 2; 32]);
                    assert!(Db::open(&path, &wrong).is_err());
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}
