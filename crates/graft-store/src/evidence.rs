use super::*;

impl GraftStore {
    pub fn write_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.object_evidence())?;
        let path = self
            .paths
            .object_evidence()
            .join(format!("{}.json", evidence.id));
        write_json(&path, evidence)?;
        self.index_evidence(evidence)?;
        Ok(path)
    }

    pub fn read_evidence(&self, id: &str) -> Result<EvidenceRecord> {
        read_json(&self.paths.object_evidence().join(format!("{id}.json")))
    }

    pub fn candidate_evidence_index(&self, candidate: &str) -> Result<Vec<String>> {
        read_evidence_index(&self.paths.object_candidate_evidence_index(), candidate)
    }

    pub fn patch_evidence_index(&self, patch: &str) -> Result<Vec<String>> {
        read_evidence_index(&self.paths.object_patch_evidence_index(), patch)
    }

    pub fn write_candidate_evidence_index(
        &self,
        candidate: &str,
        evidence: &[String],
    ) -> Result<()> {
        fs::create_dir_all(self.paths.object_candidate_evidence_index())?;
        let refs = EvidenceRefsRecord {
            owner: candidate.to_string(),
            evidence: evidence.to_vec(),
            updated_at: Some(time::OffsetDateTime::now_utc().to_string()),
        };
        write_json(
            &self
                .paths
                .object_candidate_evidence_index()
                .join(format!("{candidate}.json")),
            &refs,
        )
    }

    pub fn remove_candidate_evidence_index(&self, candidate: &str) -> Result<()> {
        match fs::remove_file(
            self.paths
                .object_candidate_evidence_index()
                .join(format!("{candidate}.json")),
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    pub fn append_candidate_evidence_index(&self, candidate: &str, evidence: &str) -> Result<()> {
        append_unique_index(
            &self.paths.object_candidate_evidence_index(),
            candidate,
            evidence,
        )
    }

    pub fn append_patch_evidence_index(&self, patch: &str, evidence: &str) -> Result<()> {
        append_unique_index(&self.paths.object_patch_evidence_index(), patch, evidence)
    }

    pub fn copy_candidate_evidence_index_to_patch(
        &self,
        candidate: &str,
        patch: &str,
    ) -> Result<Vec<String>> {
        let index = self.candidate_evidence_index(candidate)?;
        let mut copied = Vec::new();
        for old_evidence_id in index {
            let mut evidence = self.read_evidence(&old_evidence_id)?;
            if let Some(subject) = promoted_evidence_subject(&evidence.subject, candidate, patch) {
                evidence.subject = subject;
                evidence.id = graft_core::EvidenceId::new("evidence:pending");
                evidence.id = evidence_id(&evidence).map_err(StoreError::Core)?;
                self.write_evidence(&evidence)?;
                copied.push(evidence.id.to_string());
            } else {
                copied.push(old_evidence_id);
            }
        }
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        let refs = EvidenceRefsRecord {
            owner: patch.to_string(),
            evidence: copied.clone(),
            updated_at: Some(time::OffsetDateTime::now_utc().to_string()),
        };
        write_json(
            &self
                .paths
                .object_patch_evidence_index()
                .join(format!("{patch}.json")),
            &refs,
        )?;
        Ok(copied)
    }

    pub fn evidence_records_for_ids(&self, ids: &[String]) -> Result<Vec<EvidenceRecord>> {
        ids.iter().map(|id| self.read_evidence(id)).collect()
    }

    pub fn candidate_evidence_records(&self, candidate: &str) -> Result<Vec<EvidenceRecord>> {
        let ids = self.candidate_evidence_index(candidate)?;
        self.evidence_records_for_ids(&ids)
    }

    pub fn patch_evidence_records(&self, patch: &str) -> Result<Vec<EvidenceRecord>> {
        let ids = self.patch_evidence_index(patch)?;
        self.evidence_records_for_ids(&ids)
    }

    pub fn write_cache_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        self.write_evidence(evidence)
    }

    pub fn cached_evidence_for_subject(&self, subject: &str) -> Result<Vec<EvidenceRecord>> {
        self.candidate_evidence_records(subject)
    }

    pub fn registry_evidence_for_subject(&self, subject: &str) -> Result<Vec<EvidenceRecord>> {
        self.patch_evidence_records(subject)
    }

    pub fn list_registry_evidence(&self) -> Result<Vec<EvidenceRecord>> {
        let mut ids = BTreeSet::new();
        for refs in self.list_patch_evidence_refs()? {
            ids.extend(refs.evidence);
        }
        let ids = ids.into_iter().collect::<Vec<_>>();
        self.evidence_records_for_ids(&ids)
    }

    pub fn write_registry_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        self.write_evidence(evidence)
    }

    pub fn list_patch_evidence_refs(&self) -> Result<Vec<EvidenceRefsRecord>> {
        let dir = self.paths.object_patch_evidence_index();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        json_file_stems(&dir)?
            .into_iter()
            .map(|owner| read_evidence_refs_record(&dir, &owner))
            .collect()
    }

    pub fn list_evidence_body_ids(&self) -> Result<Vec<String>> {
        json_file_stems(&self.paths.object_evidence())
    }

    pub fn list_candidate_evidence_ref_owners(&self) -> Result<Vec<String>> {
        json_file_stems(&self.paths.object_candidate_evidence_index())
    }

    pub fn list_patch_evidence_ref_owners(&self) -> Result<Vec<String>> {
        json_file_stems(&self.paths.object_patch_evidence_index())
    }
}
