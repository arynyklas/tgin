//! Building an envelope allocates its one buffer, whatever the update's
//! size: no JSON value tree, no base64 strings on the side.

#[path = "../../tgin-botapi/tests/support/counting_alloc.rs"]
mod counting_alloc;

use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use counting_alloc::{Counting, thread_allocations};
use tgin_mtproto::envelope::{Meta, raw};
use tgin_mtproto::tl_json::from_json;
use tgin_mtproto::{StoredState, UpdateManager, UpdateState};

#[global_allocator]
static GLOBAL: Counting = Counting;

fn tl(text: &str) -> Bytes {
    let mut b = Vec::new();
    if let Err(e) = from_json(text, &mut b) {
        panic!("{e}: {text}");
    }
    Bytes::from(b)
}

#[test]
fn envelope_allocates_one_buffer() {
    let mut m = UpdateManager::new(StoredState {
        common: UpdateState {
            pts: 100,
            qts: 0,
            seq: 0,
            date: 1,
        },
        ..StoredState::default()
    });
    m.next_action();
    for b in m.feed_difference(tl(r#"{"_":"updates.differenceEmpty","date":1,"seq":0}"#)) {
        m.committed(b.id);
    }
    let long = "x".repeat(10_000);
    let push = tl(&format!(
        r#"{{"_":"updates","updates":[
        {{"_":"updateNewMessage","pts":101,"pts_count":1,"message":{{"_":"message","id":1,"peer_id":{{"_":"peerUser","user_id":"3"}},"date":1,"message":"short"}}}},
        {{"_":"updateNewMessage","pts":102,"pts_count":1,"message":{{"_":"message","id":2,"peer_id":{{"_":"peerUser","user_id":"3"}},"date":1,"message":"{long}"}}}}],
        "users":[],"chats":[],"date":1,"seq":0}}"#
    ));
    let batch = m
        .feed_push(push)
        .into_iter()
        .find(|b| b.updates.len() == 2)
        .expect("a batch of two");
    let meta = Meta {
        account: "main",
        layer: 230,
        update_id: 1,
        received_at: UNIX_EPOCH + Duration::from_secs(1_792_413_296),
    };
    let mut counts = Vec::new();
    for update in &batch.updates {
        let before = thread_allocations();
        let envelope = raw(&meta, update, &batch.users, &batch.chats);
        counts.push(thread_allocations() - before);
        assert!(envelope.len() > update.bytes().len());
    }
    assert_eq!(counts[0], counts[1], "independent of the update's size");
    assert!(
        counts[0] <= 2,
        "the buffer and its `Bytes` handle: {counts:?}"
    );
}
