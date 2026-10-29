use sha2::{Digest, Sha256};
use std::collections::HashMap;
const CAPACITY: usize = 4096;
const RETENTION: u64 = 600_000;
struct Entry {
    hash: [u8; 32],
    completed: Option<(u64, Result<(), String>)>,
}
#[derive(Default)]
pub(super) struct Ledger {
    entries: HashMap<String, Entry>,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    New,
    Existing,
    Rejected(String),
}
impl Ledger {
    fn expire(&mut self, now: u64) {
        self.entries.retain(|_, e| {
            e.completed
                .as_ref()
                .is_none_or(|(at, _)| now.saturating_sub(*at) < RETENTION)
        });
    }
    pub fn reserve(&mut self, runtime: &str, id: &str, payload: &[u8], now: u64) -> Decision {
        if id.split('/').next() != Some(runtime) {
            return Decision::Rejected("요청 runtime이 다릅니다.".into());
        }
        self.expire(now);
        let hash: [u8; 32] = Sha256::digest(payload).into();
        if let Some(entry) = self.entries.get(id) {
            return if entry.hash == hash {
                Decision::Existing
            } else {
                Decision::Rejected("같은 요청 번호의 내용이 다릅니다.".into())
            };
        }
        let parts: Vec<_> = id.split('/').collect();
        if parts.len() != 3 || parts[0] != runtime || uuid::Uuid::parse_str(parts[2]).is_err() {
            return Decision::Rejected("요청 runtime/nonce가 유효하지 않습니다.".into());
        }
        let Some(issued) = parts[1].parse::<u64>().ok() else {
            return Decision::Rejected("요청 발급 시각 오류".into());
        };
        if issued > now || now - issued > 30_000 {
            return Decision::Rejected(
                "요청이 만료되었습니다. 결과/현재 상태를 확인해 주세요.".into(),
            );
        }
        if self.entries.len() >= CAPACITY {
            return Decision::Rejected(
                "AGY 요청 기록이 가득 찼습니다. 상태 조회는 가능합니다.".into(),
            );
        }
        self.entries.insert(
            id.into(),
            Entry {
                hash,
                completed: None,
            },
        );
        Decision::New
    }
    pub fn complete(&mut self, id: &str, result: Result<(), String>, now: u64) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.completed = Some((now, result));
        }
    }
    pub fn lookup(&mut self, id: &str, now: u64) -> (String, Option<String>) {
        self.expire(now);
        match self.entries.get(id) {
            Some(Entry {
                completed: None, ..
            }) => ("pending".into(), None),
            Some(Entry {
                completed: Some((_, result)),
                ..
            }) => ("complete".into(), result.clone().err()),
            None => ("expired".into(), Some("결과 만료—상태 확인 필요".into())),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn id(time: u64) -> String {
        format!("runtime/{time}/{}", uuid::Uuid::new_v4())
    }
    #[test]
    fn duplicate_payload_expiry_runtime_and_saturation_contract() {
        let mut ledger = Ledger::default();
        let request = id(1);
        assert_eq!(ledger.reserve("runtime", &request, b"on", 1), Decision::New);
        assert_eq!(
            ledger.reserve("runtime", &request, b"on", 900_000),
            Decision::Existing
        );
        assert!(matches!(
            ledger.reserve("runtime", &request, b"off", 1),
            Decision::Rejected(_)
        ));
        assert!(matches!(
            ledger.reserve("new-runtime", &id(1), b"on", 1),
            Decision::Rejected(_)
        ));
        assert!(matches!(
            ledger.reserve("runtime", &id(1), b"on", 30_002),
            Decision::Rejected(_)
        ));
        for _ in 1..CAPACITY {
            assert_eq!(ledger.reserve("runtime", &id(1), b"on", 1), Decision::New);
        }
        assert!(matches!(
            ledger.reserve("runtime", &id(1), b"on", 1),
            Decision::Rejected(_)
        ));
        ledger.complete(&request, Ok(()), 2);
        assert_eq!(ledger.lookup(&request, 600_001).0, "complete");
        assert_eq!(ledger.lookup(&request, 600_002).0, "expired");
        assert_eq!(
            ledger.reserve("runtime", &id(600_002), b"on", 600_002),
            Decision::New
        );
    }
}
