use super::*;
use crate::scheduling as sched;

impl Store {
    pub fn candidate(&self, principal: &Principal, candidate_id: &str) -> Result<CandidateRecord> {
        identifier(candidate_id)?;
        let db = self.lock()?;
        let c: CandidateRecord = get(&db, "candidate", candidate_id)?;
        project(&db, principal, &c.project_id)?;
        Ok(c)
    }
    pub fn stage_candidate(
        &self,
        principal: &Principal,
        candidate_id: &str,
        expected_head: Option<String>,
        integration_sha: String,
    ) -> Result<CandidateRecord> {
        admin(principal)?;
        sha(&integration_sha)?;
        if let Some(head) = &expected_head {
            sha(head)?;
        }
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let mut c: CandidateRecord = get(&tx, "candidate", candidate_id)?;
        let (a, t) = sched::current(
            &tx,
            principal,
            &c.submission.attempt_id,
            c.submission.generation,
        )?;
        if t.state != TaskState::Validating
            || c.publication_id.is_some()
            || !sched::result_eligible(&tx, &a, &t)?
        {
            return Err(conflict("candidate is not eligible for staging"));
        }
        let p: Project = get(&tx, "project", &c.project_id)?;
        if t.spec.contract_revision != p.contract_revision
            || t.spec.policy_revision != p.policy_revision
        {
            return Err(conflict("candidate contract or policy is stale"));
        }
        if c.integration_sha.as_ref() != Some(&integration_sha) || c.expected_head != expected_head
        {
            c.decision = None;
        }
        c.integration_sha = Some(integration_sha);
        c.expected_head = expected_head;
        put(&tx, "candidate", &c.id, &c.project_id, &c)?;
        event(&tx, Some(&c.project_id), "candidate_staged", value(&c)?)?;
        tx.commit().map_err(sql)?;
        Ok(c)
    }
    pub fn prepare_publication(
        &self,
        principal: &Principal,
        candidate_id: &str,
    ) -> Result<Publication> {
        admin(principal)?;
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let mut c: CandidateRecord = get(&tx, "candidate", candidate_id)?;
        if let Some(pid) = &c.publication_id {
            let publication: Publication = get(&tx, "publication", pid)?;
            if matches!(
                publication.state.as_str(),
                "pending" | "reconciling" | "published"
            ) {
                return Ok(publication);
            }
        }
        let (a, mut t) = sched::current(
            &tx,
            principal,
            &c.submission.attempt_id,
            c.submission.generation,
        )?;
        let p: Project = get(&tx, "project", &c.project_id)?;
        if t.state != TaskState::Validating
            || t.cancel_requested
            || !sched::result_eligible(&tx, &a, &t)?
            || !a.settled
            || !a.pending_children.is_empty()
            || a.state != "result_submitted"
        {
            return Err(conflict(
                "candidate attempt is not current, complete, and publication-eligible",
            ));
        }
        if p.contract_revision != c.submission.contract_revision
            || p.policy_revision != c.submission.policy_revision
            || t.spec.contract_revision != p.contract_revision
            || t.spec.policy_revision != p.policy_revision
        {
            return Err(conflict("publication revisions invalidated"));
        }
        let deferred: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM deferred WHERE project=?1)",
                [&p.id],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if deferred {
            return Err(conflict(
                "project invalidation is awaiting publication reconciliation",
            ));
        }
        if all::<Publication>(&tx, "publication")?.iter().any(|other| {
            other.project_id == p.id && matches!(other.state.as_str(), "pending" | "reconciling")
        }) {
            return Err(conflict("project already has an unresolved publication"));
        }
        let candidate_sha = c
            .integration_sha
            .clone()
            .ok_or_else(|| conflict("candidate must be staged before publication"))?;
        let decision = c
            .decision
            .as_ref()
            .ok_or_else(|| conflict("exact candidate has no review decision"))?;
        if !decision.approve
            || decision.candidate_sha != candidate_sha
            || !c.submission.unresolved.is_empty()
            || decision
                .checks_run
                .iter()
                .any(|e| !e.passed || e.source_sha != candidate_sha)
            || (decision.checks_run.is_empty() && decision.skipped_checks.is_empty())
        {
            return Err(conflict(
                "candidate lacks applicable passing evidence and approval",
            ));
        }
        let publication = Publication {
            id: id(),
            candidate_id: c.id.clone(),
            project_id: p.id.clone(),
            task_id: t.id.clone(),
            target_branch: p.target_branch,
            expected_head: c.expected_head.clone(),
            candidate_sha,
            state: "pending".into(),
        };
        t.state = TaskState::Publishing;
        t.blocked_reason = Some("Git publication intent unresolved".into());
        c.publication_id = Some(publication.id.clone());
        put(&tx, "publication", &publication.id, &p.id, &publication)?;
        put(&tx, "task", &t.id, &p.id, &t)?;
        put(&tx, "candidate", &c.id, &p.id, &c)?;
        event(
            &tx,
            Some(&p.id),
            "publication_prepared",
            value(&publication)?,
        )?;
        tx.commit().map_err(sql)?;
        Ok(publication)
    }
    pub fn finish_publication(
        &self,
        publication_id: &str,
        observed_head: Option<&str>,
    ) -> Result<Value> {
        identifier(publication_id)?;
        if let Some(head) = observed_head {
            sha(head)?;
        }
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let mut publication: Publication = get(&tx, "publication", publication_id)?;
        if !matches!(publication.state.as_str(), "pending" | "reconciling") {
            return value(&publication);
        }
        let mut c: CandidateRecord = get(&tx, "candidate", &publication.candidate_id)?;
        let mut t: TaskRecord = get(&tx, "task", &publication.task_id)?;
        if t.state != TaskState::Publishing || c.publication_id.as_deref() != Some(publication_id) {
            return Err(conflict("publication state requires investigation"));
        }
        if observed_head == Some(publication.candidate_sha.as_str()) {
            publication.state = "published".into();
            t.state = TaskState::Accepted;
            t.integrated_sha = Some(publication.candidate_sha.clone());
            t.blocked_reason = if t.cancel_requested {
                Some("publication committed before deferred cancellation".into())
            } else {
                None
            };
            let mut a: AttemptRecord = get(&tx, "attempt", &c.submission.attempt_id)?;
            a.state = "accepted".into();
            put(&tx, "attempt", &a.id, &t.spec.project_id, &a)?;
            // Git acceptance is not proof that a remote process stopped; hold its reservation.
            put(&tx, "task", &t.id, &t.spec.project_id, &t)?;
            sched::notify_parent(&tx, &t)?;
        } else if observed_head == publication.expected_head.as_deref() {
            publication.state = "not_published".into();
            c.publication_id = None;
            c.decision = None;
            t.state = TaskState::Validating;
            t.blocked_reason =
                Some("conditional Git update did not publish; prepare and review again".into());
            put(&tx, "candidate", &c.id, &c.project_id, &c)?;
            if t.cancel_requested {
                sched::cancel(
                    &tx,
                    &mut t,
                    "deferred cancellation after unpublished intent",
                )?;
            } else {
                put(&tx, "task", &t.id, &t.spec.project_id, &t)?;
            }
        } else {
            publication.state = "reconciling".into();
            t.blocked_reason = Some(
                "local Git head differs from expected and candidate: manual investigation required"
                    .into(),
            );
            put(&tx, "task", &t.id, &t.spec.project_id, &t)?;
        }
        put(
            &tx,
            "publication",
            &publication.id,
            &publication.project_id,
            &publication,
        )?;
        event(
            &tx,
            Some(&publication.project_id),
            "publication_reconciled",
            json!({"publication":publication,"observed_head":observed_head}),
        )?;
        if publication.state != "reconciling" {
            apply_deferred(&tx, &publication.project_id)?;
            sched::refresh_dependencies(&tx)?;
        }
        let response = value(&publication)?;
        tx.commit().map_err(sql)?;
        Ok(response)
    }
    pub fn pending_publications(&self) -> Result<Vec<Publication>> {
        let db = self.lock()?;
        Ok(all::<Publication>(&db, "publication")?
            .into_iter()
            .filter(|p| matches!(p.state.as_str(), "pending" | "reconciling"))
            .collect())
    }
}
fn apply_deferred(db: &Connection, project_id: &str) -> Result<()> {
    if all::<Publication>(db, "publication")?.iter().any(|p| {
        p.project_id == project_id && matches!(p.state.as_str(), "pending" | "reconciling")
    }) {
        return Ok(());
    }
    let deferred: Option<(bool, Option<String>)> = db
        .query_row(
            "SELECT contract,requirements FROM deferred WHERE project=?1",
            [project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    if let Some((contract, requirements)) = deferred {
        let mut p: Project = get(db, "project", project_id)?;
        let policy = requirements.is_some();
        if contract {
            p.contract_revision = p
                .contract_revision
                .checked_add(1)
                .ok_or_else(|| conflict("contract revision exhausted"))?;
        }
        if let Some(requirements) = requirements {
            p.policy_revision = p
                .policy_revision
                .checked_add(1)
                .ok_or_else(|| conflict("policy revision exhausted"))?;
            p.requirements = serde_json::from_str(&requirements).map_err(serialization)?;
        }
        put(db, "project", &p.id, &p.id, &p)?;
        sched::invalidate(db, project_id, policy)?;
        db.execute("DELETE FROM deferred WHERE project=?1", [project_id])
            .map_err(sql)?;
        event(
            db,
            Some(project_id),
            "deferred_invalidation_applied",
            value(&p)?,
        )?;
    }
    Ok(())
}
