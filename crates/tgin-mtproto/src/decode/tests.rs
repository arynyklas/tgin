use std::collections::BTreeMap;

use grammers_tl_types::{Deserializable, Serializable, enums};

use super::*;
use crate::schema::Schema;
use crate::testgen::{
    self, FlagMode, Gen, MessageOracle, Oracle, Plan, Stats, hex, par_each, seed, typed,
};
use crate::tl_json::from_json;

/// Cases per constructor on every PR; `TGIN_TL_CASES` overrides.
const CASES: u32 = 1000;

fn tl(text: &str) -> Vec<u8> {
    let mut b = Vec::new();
    match from_json(text, &mut b) {
        Ok(()) => b,
        Err(e) => panic!("{e}: {text}"),
    }
}

fn ctor_named(name: &str) -> &'static Ctor {
    match API.ctor_by_name(name) {
        Some(c) => c,
        None => panic!("no {name}"),
    }
}

#[test]
fn constants_name_the_schema() {
    for (id, name) in [
        (PEER_USER, "peerUser"),
        (PEER_CHAT, "peerChat"),
        (PEER_CHANNEL, "peerChannel"),
        (UPDATE_SHORT, "updateShort"),
        (UPDATE_SHORT_MESSAGE, "updateShortMessage"),
        (UPDATE_SHORT_CHAT_MESSAGE, "updateShortChatMessage"),
        (UPDATE_SHORT_SENT_MESSAGE, "updateShortSentMessage"),
    ] {
        assert_eq!(ctor_named(name).id, id, "{name}");
    }
    let peer = API.type_index("Peer").map(|t| API.type_def(t).ctors.len());
    assert_eq!(
        peer,
        Some(3),
        "Peer has exactly the three constructors `peer` reads"
    );
}

/// The box every `Update` constructor and short form is in. The sequenced
/// ones are grammers 0.11's `PtsInfo::from_update` list
/// (grammers-session `message_box/adaptor.rs`), plus the five layer-230
/// updates with a `qts` field that grammers leaves out.
#[test]
fn box_kinds_match_grammers_adaptor() {
    let common = [
        "updateNewMessage",
        "updateDeleteMessages",
        "updateReadHistoryInbox",
        "updateReadHistoryOutbox",
        "updateWebPage",
        "updateReadMessagesContents",
        "updateEditMessage",
        "updateFolderPeers",
        "updatePinnedMessages",
        "updateShortMessage",
        "updateShortChatMessage",
        "updateShortSentMessage",
    ];
    let channel = [
        "updateChannelTooLong",
        "updateNewChannelMessage",
        "updateReadChannelInbox",
        "updateDeleteChannelMessages",
        "updateEditChannelMessage",
        "updateChannelWebPage",
        "updatePinnedChannelMessages",
    ];
    let qts = [
        "updateNewEncryptedMessage",
        "updateChatParticipant",
        "updateChannelParticipant",
        "updateBotStopped",
        "updateBotChatInviteRequester",
        "updateBotChatBoost",
        "updateBotMessageReaction",
        "updateBotMessageReactions",
        "updateBotNewBusinessMessage",
        "updateBotEditBusinessMessage",
        "updateBotDeleteBusinessMessage",
        "updateBotPurchasedPaidMedia",
        // Not in grammers' list; each has a `qts` field.
        "updateMessagePollVote",
        "updateBotBusinessConnect",
        "updateManagedBot",
        "updateBotGuestChatQuery",
        "updateBotStarsSubscription",
    ];
    let mut want: BTreeMap<&str, Seq> = BTreeMap::new();
    want.extend(common.map(|n| (n, Seq::Common)));
    want.extend(channel.map(|n| (n, Seq::Channel)));
    want.extend(qts.map(|n| (n, Seq::Qts)));
    let mut wrong = Vec::new();
    for c in API.ctors {
        let Some(t) = c.ty else { continue };
        let t = API.type_def(t).name;
        if t != "Update" && t != "Updates" {
            assert_eq!(c.seq, Seq::None, "{}", c.name);
            continue;
        }
        let w = want.get(c.name).copied().unwrap_or(Seq::None);
        if c.seq != w {
            wrong.push(format!("{}: {:?}, want {w:?}", c.name, c.seq));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
    for n in want.keys() {
        assert!(API.ctor_by_name(n).is_some(), "{n} is not in layer 230");
    }
}

#[test]
fn facts_of_messages() {
    // A private message from a user, with a command.
    let b = tl(
        r#"{"_":"updateNewMessage","pts":10,"pts_count":1,"message":{"_":"message","id":5,
        "peer_id":{"_":"peerUser","user_id":"42"},"date":0,"message":"/start@my_bot hi"}}"#,
    );
    let f = facts(&b);
    assert_eq!(
        f,
        Ok(UpdateFacts {
            constructor: "updateNewMessage",
            box_kind: BoxKind::Common,
            pts: Some(10),
            pts_count: Some(1),
            qts: None,
            peer: Some(PeerId::User(42)),
            sender: Some(42),
            text: Some("/start@my_bot hi"),
            command: Some("start@my_bot"),
        })
    );
    // The text is borrowed from the input.
    let text = f.ok().and_then(|f| f.text).unwrap_or_default();
    let base = b.as_ptr() as usize;
    assert!((base..base + b.len()).contains(&(text.as_ptr() as usize)));

    // An outgoing private message has no known sender; a channel post's
    // sender is the channel, not a user.
    let b = tl(
        r#"{"_":"updateNewMessage","pts":10,"pts_count":1,"message":{"_":"message","out":true,"id":5,
        "peer_id":{"_":"peerUser","user_id":"42"},"date":0,"message":"x"}}"#,
    );
    assert_eq!(facts(&b).map(|f| f.sender), Ok(None));
    let b = tl(
        r#"{"_":"updateNewChannelMessage","pts":7,"pts_count":2,"message":{"_":"message","id":5,
        "from_id":{"_":"peerChannel","channel_id":"9"},"peer_id":{"_":"peerChannel","channel_id":"9"},
        "date":0,"message":"/x"}}"#,
    );
    let f = facts(&b);
    assert_eq!(
        f.map(|f| (f.box_kind, f.peer, f.sender, f.command)),
        Ok((
            BoxKind::Channel(9),
            Some(PeerId::Channel(9)),
            None,
            Some("x")
        ))
    );
    // A group message from a user.
    let b = tl(
        r#"{"_":"updateEditMessage","pts":1,"pts_count":1,"message":{"_":"message","id":5,
        "from_id":{"_":"peerUser","user_id":"3"},"peer_id":{"_":"peerChat","chat_id":"8"},
        "date":0,"message":"hello /x"}}"#,
    );
    let f = facts(&b);
    assert_eq!(
        f.map(|f| (f.peer, f.sender, f.command)),
        Ok((Some(PeerId::Chat(8)), Some(3), None))
    );
    // Telegram's updateNewChannelMessage(messageEmpty) without a peer: no
    // channel, so no box (as in grammers).
    let b = tl(
        r#"{"_":"updateNewChannelMessage","pts":7,"pts_count":1,"message":{"_":"messageEmpty","id":1}}"#,
    );
    assert_eq!(
        facts(&b).map(|f| (f.box_kind, f.pts)),
        Ok((BoxKind::None, Some(7)))
    );
}

#[test]
fn facts_of_short_forms() {
    let b = tl(
        r#"{"_":"updateShortMessage","id":1,"user_id":"5","message":"/help","pts":3,"pts_count":1,"date":0}"#,
    );
    assert_eq!(
        facts(&b).map(|f| (f.constructor, f.box_kind, f.peer, f.sender, f.command)),
        Ok((
            "updateShortMessage",
            BoxKind::Common,
            Some(PeerId::User(5)),
            Some(5),
            Some("help")
        ))
    );
    let b = tl(
        r#"{"_":"updateShortMessage","out":true,"id":1,"user_id":"5","message":"x","pts":3,"pts_count":1,"date":0}"#,
    );
    assert_eq!(
        facts(&b).map(|f| (f.peer, f.sender)),
        Ok((Some(PeerId::User(5)), None))
    );
    let b = tl(
        r#"{"_":"updateShortChatMessage","id":1,"from_id":"6","chat_id":"7","message":"t","pts":3,"pts_count":1,"date":0}"#,
    );
    assert_eq!(
        facts(&b).map(|f| (f.peer, f.sender, f.text)),
        Ok((Some(PeerId::Chat(7)), Some(6), Some("t")))
    );
    // updateShort reports the update it wraps.
    let b = tl(
        r#"{"_":"updateShort","update":{"_":"updateChannelTooLong","channel_id":"4"},"date":0}"#,
    );
    assert_eq!(
        facts(&b).map(|f| (f.constructor, f.box_kind, f.pts, f.peer)),
        Ok((
            "updateChannelTooLong",
            BoxKind::Channel(4),
            None,
            Some(PeerId::Channel(4))
        ))
    );
    // `updateShort.update` is an `Update`: a short form inside it is refused,
    // as `container` and the codec refuse it.
    let short_message = tl(
        r#"{"_":"updateShortMessage","id":1,"user_id":"5","message":"x","pts":3,"pts_count":1,"date":0}"#,
    );
    let wrap =
        |inner: &[u8]| [&UPDATE_SHORT.to_le_bytes()[..], inner, &0i32.to_le_bytes()].concat();
    for nested in [wrap(&short_message), wrap(&wrap(&short_message))] {
        let got = match API.ctor_by_id(u32::from_le_bytes([
            nested[4], nested[5], nested[6], nested[7],
        ])) {
            Some(c) => c.name,
            None => "",
        };
        let want = Err(TlError::WrongType {
            want: "Update",
            got,
        });
        assert_eq!(facts(&nested), want);
        assert!(container(&nested).is_err());
        assert!(crate::tl_json::to_json(&nested, &mut String::new()).is_err());
    }
    // Neither a container nor another type is an update.
    let b = tl(r#"{"_":"updatesTooLong"}"#);
    assert_eq!(
        facts(&b),
        Err(TlError::WrongType {
            want: "Update",
            got: "updatesTooLong"
        })
    );
    let b = tl(r#"{"_":"inputPeerEmpty"}"#);
    assert!(facts(&b).is_err());
    // A trailing byte is refused.
    let mut b = tl(r#"{"_":"updateUser","user_id":"1"}"#);
    assert!(facts(&b).is_ok());
    b.push(0);
    assert!(matches!(facts(&b), Err(TlError::Trailing { .. })));
}

/// The facts the oracle's typed fields imply, by the rules of
/// [`UpdateFacts`]'s docs.
#[derive(Debug, PartialEq, Eq)]
struct Expect {
    constructor: &'static str,
    box_kind: BoxKind,
    pts: Option<i32>,
    pts_count: Option<i32>,
    qts: Option<i32>,
    peer: Option<PeerId>,
    sender: Option<i64>,
    text: Option<String>,
}

fn expect(raw: &[u8]) -> Option<Expect> {
    let id = u32::from_le_bytes(raw.get(..4)?.try_into().ok()?);
    let ctor = API.ctor_by_id(id)?;
    let o: Oracle = typed::update_oracle(id, &raw[4..])?;
    if let Some(inner) = &o.inner {
        return expect(inner);
    }
    let m: Option<&MessageOracle> = o.message.as_ref();
    let m_peer = m.and_then(|m| m.peer);
    let channel = o.channel_id.or(match m_peer {
        Some(PeerId::Channel(c)) => Some(c),
        _ => None,
    });
    let user = |p: Option<PeerId>| match p {
        Some(PeerId::User(u)) => Some(u),
        _ => None,
    };
    let sender = match m {
        Some(m) if m.from.is_some() => user(m.from),
        Some(m) if !m.out => user(m.peer),
        Some(_) => None,
        None => user(o.from)
            .or(o.from_user)
            .or(if o.out == Some(true) { None } else { o.user_id }),
    };
    Some(Expect {
        constructor: ctor.name,
        box_kind: match ctor.seq {
            Seq::None => BoxKind::None,
            Seq::Common => BoxKind::Common,
            Seq::Qts => BoxKind::Qts,
            Seq::Channel => channel.map_or(BoxKind::None, BoxKind::Channel),
        },
        pts: o.pts,
        pts_count: o.pts_count,
        qts: o.qts,
        peer: m_peer
            .or(o.peer)
            .or(o.channel_id.map(PeerId::Channel))
            .or(o.chat_id.map(PeerId::Chat))
            .or(o.user_id.map(PeerId::User)),
        sender,
        text: m.map_or(o.text.clone(), |m| m.text.clone()),
    })
}

/// The constructors of the named boxed types.
fn of_types(s: &Schema, names: &[&str]) -> Vec<usize> {
    (0..s.ctors.len())
        .filter(|&i| {
            s.ctors[i]
                .ty
                .is_some_and(|t| names.contains(&s.type_def(t).name))
        })
        .collect()
}

/// Generates `cases` valid-UTF-8 values of constructor `idx` and passes
/// each to `check`; findings are capped at 3 per constructor.
fn each_case(
    idx: usize,
    cases: u32,
    plan: &Plan,
    found: &mut Vec<String>,
    check: &dyn Fn(&[u8]) -> Result<(), String>,
) {
    let c = &API.ctors[idx];
    let mut st = Stats::default();
    let mut failed = 0;
    for case in 0..cases {
        let mut g = Gen::new(&API, plan, seed(c.id, case), FlagMode::of(case), &mut st);
        g.utf8_only = true;
        let mut raw = Vec::new();
        g.ctor(&mut raw, idx, true, 0);
        if let Err(e) = check(&raw) {
            if failed < 3 {
                found.push(format!(
                    "{} case {case}: {e}\n  input {}",
                    c.name,
                    hex(&raw)
                ));
            }
            failed += 1;
        }
    }
}

fn assert_none(what: &str, findings: &[String], n: usize, cases: u32) {
    println!(
        "{what}: {n} constructors, {cases} cases each, {} findings",
        findings.len()
    );
    for f in findings.iter().take(30) {
        println!("FINDING {f}");
    }
    assert!(findings.is_empty(), "{what}: {} findings", findings.len());
}

/// For random values of every `Update` constructor and of the four short
/// `Updates` forms, `facts` agrees with grammers' typed decode on pts,
/// pts_count, qts, the channel (box), the peer, the sender and the text.
#[test]
fn facts_match_typed_decode() {
    let plan = Plan::new(&API);
    let mut ctors = of_types(&API, &["Update"]);
    ctors.extend(
        [
            "updateShort",
            "updateShortMessage",
            "updateShortChatMessage",
            "updateShortSentMessage",
        ]
        .map(|n| {
            API.ctors
                .iter()
                .position(|c| c.name == n)
                .unwrap_or(usize::MAX)
        }),
    );
    assert!(ctors.len() > 170);
    let cases = testgen::cases(CASES);
    let findings = par_each(&ctors, |idx, found| {
        each_case(idx, cases, &plan, found, &|raw| {
            let want = expect(raw).ok_or("grammers cannot read it")?;
            let got = facts(raw).map_err(|e| format!("facts: {e}"))?;
            let got = Expect {
                constructor: got.constructor,
                box_kind: got.box_kind,
                pts: got.pts,
                pts_count: got.pts_count,
                qts: got.qts,
                peer: got.peer,
                sender: got.sender,
                text: got.text.map(str::to_owned),
            };
            if got != want {
                return Err(format!("facts {got:?}\n  typed {want:?}"));
            }
            Ok(())
        });
    });
    assert_none("facts_match_typed_decode", &findings, ctors.len(), cases);
}

fn offset(whole: &[u8], part: &[u8]) -> usize {
    part.as_ptr() as usize - whole.as_ptr() as usize
}

/// Concatenates `parts` (sub-slices of `whole`, in order, each starting
/// where the previous ended) with the bytes before the first and after
/// the last; `Err` when a part does not start where the previous ended.
fn reassemble(whole: &[u8], parts: &[&[u8]]) -> Result<Vec<u8>, String> {
    let Some(first) = parts.first() else {
        return Ok(whole.to_vec());
    };
    let mut at = offset(whole, first);
    let mut out = whole[..at].to_vec();
    for p in parts {
        if offset(whole, p) != at {
            return Err(format!("a part starts at {}, not {at}", offset(whole, p)));
        }
        out.extend_from_slice(p);
        at += p.len();
    }
    out.extend_from_slice(&whole[at..]);
    Ok(out)
}

/// For every `Updates` constructor: the update, users and chats slices
/// concatenate back to the container's bytes, the head and tail are only
/// the fixed fields around them, and every slice is what grammers reads.
#[test]
fn container_slices_reassemble() {
    let plan = Plan::new(&API);
    let ctors = of_types(&API, &["Updates"]);
    assert_eq!(ctors.len(), 7);
    let cases = testgen::cases(CASES);
    let findings = par_each(&ctors, |idx, found| {
        each_case(idx, cases, &plan, found, &|raw| {
            let c = container(raw).map_err(|e| format!("container: {e}"))?;
            let typed = enums::Updates::from_bytes(raw).map_err(|e| format!("grammers: {e}"))?;
            let short = c.constructor.starts_with("updateShort");
            if short {
                if c.updates != [raw] || c.users.is_some() || c.chats.is_some() {
                    return Err("a short form is its own one update".into());
                }
                return facts(raw).map(drop).map_err(|e| format!("facts: {e}"));
            }
            let mut parts: Vec<&[u8]> = c.updates.clone();
            parts.extend(c.users);
            parts.extend(c.chats);
            if reassemble(raw, &parts)? != raw {
                return Err("the slices do not reassemble".into());
            }
            let (ups, users, chats, head, tail) = match &typed {
                enums::Updates::Updates(u) => {
                    (&u.updates, &u.users, &u.chats, (u.date, None, u.seq), 8)
                }
                enums::Updates::Combined(u) => (
                    &u.updates,
                    &u.users,
                    &u.chats,
                    (u.date, Some(u.seq_start), u.seq),
                    12,
                ),
                enums::Updates::TooLong => {
                    return if c.updates.is_empty() && raw.len() == 4 {
                        Ok(())
                    } else {
                        Err("updatesTooLong has no parts".into())
                    };
                }
                _ => return Err("an unexpected form".into()),
            };
            if (c.date, c.seq_start, c.seq) != (Some(head.0), head.1, Some(head.2)) {
                return Err("date/seq differ".into());
            }
            // Head: id, vector id, count; tail: the trailing ints.
            let first = c
                .updates
                .first()
                .map_or(c.users.map_or(0, |u| offset(raw, u)), |u| offset(raw, u));
            let end = c.chats.map_or(0, |ch| offset(raw, ch) + ch.len());
            if first != 12 || raw.len() - end != tail {
                return Err(format!("head {first} / tail {} bytes", raw.len() - end));
            }
            if c.updates.len() != ups.len()
                || c.updates.iter().zip(ups).any(|(s, u)| u.to_bytes() != *s)
                || c.users != Some(&users.to_bytes()[..])
                || c.chats != Some(&chats.to_bytes()[..])
            {
                return Err("a slice is not grammers' value".into());
            }
            for u in &c.updates {
                facts(u).map_err(|e| format!("facts of an inner update: {e}"))?;
            }
            Ok(())
        });
    });
    assert_none("container_slices_reassemble", &findings, ctors.len(), cases);
}

/// For every `updates.Difference` and `updates.ChannelDifference` form,
/// every slice and the state are grammers' typed values.
#[test]
fn difference_slices_match_typed() {
    use enums::updates::{ChannelDifference as Cd, Difference as D};

    fn eq<T: Serializable>(slices: &[&[u8]], typed: &[T]) -> bool {
        slices.len() == typed.len() && slices.iter().zip(typed).all(|(s, t)| t.to_bytes() == *s)
    }
    fn raw<T: Serializable>(slice: Option<&[u8]>, typed: &T) -> bool {
        slice == Some(&typed.to_bytes()[..])
    }
    fn state(d: &Difference<'_>, s: &enums::updates::State) -> bool {
        let enums::updates::State::State(s) = s;
        raw(d.state, &enums::updates::State::State(s.clone()))
            && (d.pts, d.qts, d.date, d.seq)
                == (Some(s.pts), Some(s.qts), Some(s.date), Some(s.seq))
    }

    let plan = Plan::new(&API);
    let ctors = of_types(&API, &["updates.Difference", "updates.ChannelDifference"]);
    assert_eq!(ctors.len(), 7);
    let cases = testgen::cases(CASES);
    let findings = par_each(&ctors, |idx, found| {
        each_case(idx, cases, &plan, found, &|bytes| {
            let d = difference(bytes).map_err(|e| format!("difference: {e}"))?;
            let ok = if d.constructor.starts_with("updates.channel") {
                match Cd::from_bytes(bytes).map_err(|e| format!("grammers: {e}"))? {
                    Cd::Empty(t) => d.is_final == t.r#final && d.pts == Some(t.pts),
                    Cd::TooLong(t) => {
                        let dialog_pts = match &t.dialog {
                            enums::Dialog::Dialog(dialog) => dialog.pts,
                            _ => None,
                        };
                        d.is_final == t.r#final
                            && raw(d.dialog, &t.dialog)
                            && d.pts == dialog_pts
                            && eq(&d.new_messages, &t.messages)
                            && raw(d.chats, &t.chats)
                            && raw(d.users, &t.users)
                    }
                    Cd::Difference(t) => {
                        d.is_final == t.r#final
                            && d.pts == Some(t.pts)
                            && eq(&d.new_messages, &t.new_messages)
                            && eq(&d.other_updates, &t.other_updates)
                            && raw(d.chats, &t.chats)
                            && raw(d.users, &t.users)
                    }
                }
            } else {
                match D::from_bytes(bytes).map_err(|e| format!("grammers: {e}"))? {
                    D::Empty(t) => {
                        d.date == Some(t.date) && d.seq == Some(t.seq) && d.pts.is_none()
                    }
                    D::TooLong(t) => d.pts == Some(t.pts) && d.state.is_none(),
                    D::Difference(t) => {
                        eq(&d.new_messages, &t.new_messages)
                            && eq(&d.new_encrypted_messages, &t.new_encrypted_messages)
                            && eq(&d.other_updates, &t.other_updates)
                            && raw(d.chats, &t.chats)
                            && raw(d.users, &t.users)
                            && state(&d, &t.state)
                    }
                    D::Slice(t) => {
                        eq(&d.new_messages, &t.new_messages)
                            && eq(&d.new_encrypted_messages, &t.new_encrypted_messages)
                            && eq(&d.other_updates, &t.other_updates)
                            && raw(d.chats, &t.chats)
                            && raw(d.users, &t.users)
                            && state(&d, &t.intermediate_state)
                    }
                }
            };
            if !ok {
                return Err(format!("differs from grammers: {d:?}"));
            }
            for m in &d.new_messages {
                enums::Message::from_bytes(m).map_err(|e| format!("a message slice: {e}"))?;
            }
            for u in &d.other_updates {
                facts(u).map_err(|e| format!("facts of an other update: {e}"))?;
            }
            Ok(())
        });
    });
    assert_none(
        "difference_slices_match_typed",
        &findings,
        ctors.len(),
        cases,
    );
}

/// One Bot API dialog-id mapping: the facts' peers and the session's peer
/// rows agree for every kind.
#[test]
fn peer_ids_and_peer_rows_share_one_mapping() {
    use crate::{Peer, PeerKind};
    for (id, kind) in [
        (
            PeerId::User(7),
            PeerKind::User {
                bot: None,
                is_self: None,
            },
        ),
        (PeerId::Chat(7), PeerKind::Chat),
        (PeerId::Channel(7), PeerKind::Channel { kind: None }),
    ] {
        let row = Peer {
            id: 7,
            kind,
            access_hash: None,
        };
        assert_eq!(id.dialog_id(), row.dialog_id(), "{id:?}");
    }
    assert_eq!(PeerId::User(7).dialog_id(), 7);
    assert_eq!(PeerId::Chat(7).dialog_id(), -7);
    assert_eq!(PeerId::Channel(7).dialog_id(), -1_000_000_000_007);
    assert_eq!(channel_dialog_id(1_234_567_890), -1_001_234_567_890);
}
