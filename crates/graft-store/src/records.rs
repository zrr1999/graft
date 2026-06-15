use super::*;

impl GraftStore {
    pub fn write_candidate(&self, candidate: &GraftCandidate) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.cache_candidates())?;
        let path = self
            .paths
            .cache_candidates()
            .join(format!("{}.json", candidate.id));
        write_json(&path, candidate)?;
        self.write_candidate_evidence_index(candidate.id.as_str(), &[])?;
        Ok(path)
    }

    pub fn remove_candidate(&self, id: &str) -> Result<()> {
        match fs::remove_file(self.paths.cache_candidates().join(format!("{id}.json"))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    pub fn read_candidate(&self, id: &str) -> Result<GraftCandidate> {
        read_current_json(&self.paths.cache_candidates().join(format!("{id}.json")))
    }

    pub fn list_candidates(&self) -> Result<Vec<GraftCandidate>> {
        read_json_records(&self.paths.cache_candidates())
    }

    pub fn write_cache_relation(&self, relation: &PatchRelation) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.cache_relations())?;
        let path = self
            .paths
            .cache_relations()
            .join(format!("{}.json", relation.id));
        write_json(&path, relation)?;
        Ok(path)
    }

    pub fn cached_relations_for_subject(&self, subject: &str) -> Result<Vec<PatchRelation>> {
        read_relation_records(&self.paths.cache_relations(), subject)
    }

    pub fn write_patch_evidence_refs(&self, refs: &EvidenceRefsRecord) -> Result<()> {
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        write_json(
            &self
                .paths
                .object_patch_evidence_index()
                .join(format!("{}.json", refs.owner)),
            refs,
        )
    }

    pub fn write_patch(&self, patch: &PatchRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.registry_patches())?;
        let path = self
            .paths
            .registry_patches()
            .join(format!("{}.json", patch.id));
        write_json(&path, patch)?;
        self.index_patch(patch)?;
        Ok(path)
    }

    pub fn write_patch_object(&self, patch: &PatchRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.object_patches())?;
        let path = self
            .paths
            .object_patches()
            .join(format!("{}.json", patch.id));
        write_json(&path, patch)?;
        Ok(path)
    }

    pub fn write_ref(&self, name: &str, value: &str) -> Result<PathBuf> {
        let path = self.paths.refs().join(name.trim_start_matches('/'));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{value}\n"))?;
        Ok(path)
    }

    pub fn read_patch(&self, id: &str) -> Result<PatchRecord> {
        read_current_json(&self.paths.registry_patches().join(format!("{id}.json")))
    }

    pub fn list_patches(&self) -> Result<Vec<PatchRecord>> {
        read_json_records(&self.paths.registry_patches())
    }

    pub fn write_relation(&self, relation: &PatchRelation) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.registry_relations())?;
        let path = self
            .paths
            .registry_relations()
            .join(format!("{}.json", relation.id));
        write_json(&path, relation)?;
        Ok(path)
    }

    pub fn list_relations(&self) -> Result<Vec<PatchRelation>> {
        read_json_records(&self.paths.registry_relations())
    }

    pub fn write_promotion(&self, promotion: &PromotionRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.registry_promotions())?;
        let path = self
            .paths
            .registry_promotions()
            .join(format!("{}.json", promotion.id));
        write_json(&path, promotion)?;
        Ok(path)
    }

    pub fn list_promotions(&self) -> Result<Vec<PromotionRecord>> {
        read_json_records(&self.paths.registry_promotions())
    }
}
