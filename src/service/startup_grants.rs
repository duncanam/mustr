//! Ephemeral one-use Join authority owned by a single service actor.

use std::{
    collections::BTreeMap,
    result::Result,
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::protocol::{LaunchId, SpaceId, StartupGrant, StartupGrantToken};

use super::IssuedStartupGrant;

const GRANT_VERSION: u32 = 1;
const GRANT_LIFETIME: Duration = Duration::from_secs(60);

struct Entry {
    launch_id: LaunchId,
    role: String,
    pane_id: String,
    space_id: SpaceId,
    deadline: Instant,
    spent: bool,
    admission_pending: bool,
}

pub(super) struct Ledger {
    entries: BTreeMap<StartupGrantToken, Entry>,
}

impl Ledger {
    pub(super) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub(super) fn issue(
        &mut self,
        launch_id: LaunchId,
        role: String,
        pane_id: String,
        space_id: SpaceId,
        issued_at: Instant,
    ) -> Result<IssuedStartupGrant, &'static str> {
        if launch_id.as_str().is_empty()
            || role.is_empty()
            || pane_id.is_empty()
            || space_id.as_str().is_empty()
        {
            return Err("startup grant bindings must not be empty");
        }

        let deadline = issued_at + GRANT_LIFETIME;
        let token = loop {
            let candidate = StartupGrantToken::new(Uuid::new_v4().simple().to_string());
            if !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let handoff = StartupGrant {
            version: GRANT_VERSION,
            launch_id: launch_id.clone(),
            token: token.clone(),
        };
        self.entries.insert(
            token,
            Entry {
                launch_id,
                role,
                pane_id,
                space_id,
                deadline,
                spent: false,
                admission_pending: false,
            },
        );
        Ok(IssuedStartupGrant {
            handoff,
            issued_at,
            deadline,
        })
    }

    pub(super) fn consume(
        &mut self,
        grant: StartupGrant,
        role: &str,
        pane_id: &str,
        space_id: &SpaceId,
        now: Instant,
        reset_frozen: bool,
    ) -> Result<(), &'static str> {
        let entry = self
            .entries
            .get_mut(&grant.token)
            .ok_or("unknown or expired startup grant")?;
        if entry.spent {
            return Err("startup grant was already consumed");
        }
        // Recognition is irreversible even when any later binding check rejects.
        entry.spent = true;
        if grant.version != GRANT_VERSION {
            return Err("unsupported startup grant version");
        }
        if grant.launch_id != entry.launch_id {
            return Err("startup grant launch does not match");
        }
        if role != entry.role {
            return Err("startup grant role does not match");
        }
        if pane_id != entry.pane_id {
            return Err("startup grant pane does not match");
        }
        if space_id != &entry.space_id {
            return Err("startup grant space does not match");
        }
        if now >= entry.deadline {
            return Err("startup grant expired");
        }
        if reset_frozen {
            return Err("space is frozen; startup grant cannot be consumed");
        }
        entry.admission_pending = true;
        Ok(())
    }

    pub(super) fn authorize_join(
        &mut self,
        grant: &StartupGrant,
        role: &str,
        pane_id: &str,
        space_id: &SpaceId,
        now: Instant,
        reset_frozen: bool,
    ) -> Result<(), &'static str> {
        let entry = self
            .entries
            .remove(&grant.token)
            .ok_or("unknown or expired startup grant")?;
        if !entry.spent || !entry.admission_pending {
            return Err("startup grant was not consumed at the Join boundary");
        }
        if grant.version != GRANT_VERSION {
            return Err("unsupported startup grant version");
        }
        if grant.launch_id != entry.launch_id {
            return Err("startup grant launch does not match");
        }
        if role != entry.role {
            return Err("startup grant role does not match");
        }
        if pane_id != entry.pane_id {
            return Err("startup grant pane does not match");
        }
        if space_id != &entry.space_id {
            return Err("startup grant space does not match");
        }
        if now >= entry.deadline {
            return Err("startup grant expired before Join admission");
        }
        if reset_frozen {
            return Err("space became frozen before Join admission");
        }
        Ok(())
    }

    pub(super) fn retire(&mut self, token: &StartupGrantToken) {
        self.entries.remove(token);
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(super) fn prune_expired(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.deadline > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_expire_at_the_exact_monotonic_deadline() {
        let mut ledger = Ledger::new();
        let space = SpaceId::new("w1");
        let issued = ledger
            .issue(
                LaunchId::new("launch-before".into()),
                "reviewer".into(),
                "w1:p9".into(),
                space.clone(),
                Instant::now(),
            )
            .unwrap();
        assert_eq!(issued.deadline - issued.issued_at, GRANT_LIFETIME);
        let before_deadline = issued.deadline - Duration::from_nanos(1);
        ledger
            .consume(
                issued.handoff.clone(),
                "reviewer",
                "w1:p9",
                &space,
                before_deadline,
                false,
            )
            .unwrap();
        assert_eq!(
            ledger
                .authorize_join(
                    &issued.handoff,
                    "reviewer",
                    "w1:p9",
                    &space,
                    issued.deadline,
                    false,
                )
                .unwrap_err(),
            "startup grant expired before Join admission"
        );

        let expired = ledger
            .issue(
                LaunchId::new("launch-expired".into()),
                "reviewer".into(),
                "w1:p9".into(),
                space.clone(),
                issued.deadline,
            )
            .unwrap();
        assert_eq!(
            ledger
                .consume(
                    expired.handoff.clone(),
                    "reviewer",
                    "w1:p9",
                    &space,
                    expired.deadline,
                    false,
                )
                .unwrap_err(),
            "startup grant expired"
        );
        assert_eq!(
            ledger
                .consume(
                    expired.handoff,
                    "reviewer",
                    "w1:p9",
                    &space,
                    expired.deadline,
                    false,
                )
                .unwrap_err(),
            "startup grant was already consumed"
        );
    }

    #[test]
    fn pending_grant_is_rejected_if_space_freezes_before_join_admission() {
        let mut ledger = Ledger::new();
        let space = SpaceId::new("w1");
        let issued = ledger
            .issue(
                LaunchId::new("launch-freeze-race".into()),
                "reviewer".into(),
                "w1:p9".into(),
                space.clone(),
                Instant::now(),
            )
            .unwrap();
        let grant = issued.handoff;
        let admitted_at = issued.issued_at + Duration::from_secs(1);
        ledger
            .consume(
                grant.clone(),
                "reviewer",
                "w1:p9",
                &space,
                admitted_at,
                false,
            )
            .unwrap();

        assert_eq!(
            ledger
                .authorize_join(&grant, "reviewer", "w1:p9", &space, admitted_at, true,)
                .unwrap_err(),
            "space became frozen before Join admission"
        );
        assert_eq!(
            ledger
                .authorize_join(&grant, "reviewer", "w1:p9", &space, admitted_at, false,)
                .unwrap_err(),
            "unknown or expired startup grant"
        );
    }
}
