//! `decode::facts` never allocates: the schema is static, the walk keeps
//! its state on the stack, and the text is borrowed from the input.

#[path = "../../tgin-botapi/tests/support/counting_alloc.rs"]
mod counting_alloc;

use counting_alloc::{Counting, thread_allocations};
use tgin_mtproto::decode::{BoxKind, PeerId, facts};
use tgin_mtproto::tl_json::from_json;

#[global_allocator]
static GLOBAL: Counting = Counting;

fn tl(text: &str) -> Vec<u8> {
    let mut b = Vec::new();
    if let Err(e) = from_json(text, &mut b) {
        panic!("{e}: {text}");
    }
    b
}

#[test]
fn facts_allocate_nothing() {
    // Every walk path: a message with flags, peers, a long text and a
    // media caption skipped field by field; a channel message; the short
    // forms; updateShort; a qts update; an update with vectors to skip.
    let long = "x".repeat(300);
    let updates = [
        tl(&format!(
            r#"{{"_":"updateNewMessage","pts":10,"pts_count":1,"message":{{"_":"message","silent":true,"id":5,
            "from_id":{{"_":"peerUser","user_id":"3"}},"peer_id":{{"_":"peerChat","chat_id":"8"}},
            "fwd_from":{{"_":"messageFwdHeader","from_name":"n","date":1}},"date":0,
            "message":"/start@my_bot {long}","entities":[{{"_":"messageEntityBold","offset":0,"length":1}}],
            "views":4,"forwards":0}}}}"#
        )),
        tl(
            r#"{"_":"updateNewChannelMessage","pts":7,"pts_count":1,"message":{"_":"message","id":5,
            "peer_id":{"_":"peerChannel","channel_id":"9"},"date":0,"message":"post"}}"#,
        ),
        tl(
            r#"{"_":"updateShortMessage","id":1,"user_id":"5","message":"/help","pts":3,"pts_count":1,"date":0}"#,
        ),
        tl(
            r#"{"_":"updateShortChatMessage","id":1,"from_id":"6","chat_id":"7","message":"t","pts":3,"pts_count":1,"date":0,
            "entities":[{"_":"messageEntityCode","offset":0,"length":1}]}"#,
        ),
        tl(
            r#"{"_":"updateShort","update":{"_":"updateUserStatus","user_id":"1","status":{"_":"userStatusOnline","expires":5}},"date":0}"#,
        ),
        tl(r#"{"_":"updateBotStopped","user_id":"2","date":0,"stopped":true,"qts":11}"#),
        tl(
            r#"{"_":"updateDeleteChannelMessages","channel_id":"9","messages":[1,2,3],"pts":8,"pts_count":3}"#,
        ),
    ];
    // Sanity: the facts are what they should be.
    let f = facts(&updates[0]).map(|f| (f.peer, f.sender, f.command));
    assert_eq!(
        f,
        Ok((Some(PeerId::Chat(8)), Some(3), Some("start@my_bot")))
    );
    assert_eq!(
        facts(&updates[5]).map(|f| (f.box_kind, f.qts)),
        Ok((BoxKind::Qts, Some(11)))
    );

    let before = thread_allocations();
    let mut seen = 0usize;
    for _ in 0..1000 {
        for u in &updates {
            let f = facts(u);
            seen += usize::from(
                f.is_ok_and(|f| f.pts.is_some() || f.qts.is_some() || f.peer.is_some()),
            );
        }
        // A refused update allocates nothing either.
        seen += usize::from(facts(&updates[0][..20]).is_err());
    }
    let allocations = thread_allocations() - before;
    assert_eq!(seen, 8000);
    assert_eq!(allocations, 0, "facts allocated {allocations} times");
}
