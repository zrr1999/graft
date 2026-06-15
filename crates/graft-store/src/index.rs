use super::*;

impl GraftStore {
    pub fn search_patches_by_plan(&self, plan: &PlanId) -> Result<Vec<String>> {
        if !self.paths.index().exists() {
            return Ok(Vec::new());
        }
        let conn = Connection::open(self.paths.index())?;
        let plan = serde_json::to_string(plan)?;
        let mut statement = conn.prepare(
            "SELECT patch_id FROM patch_constraints WHERE constraint_id = ?1 ORDER BY patch_id",
        )?;
        let rows = statement.query_map([plan], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub(crate) fn ensure_store_schema_version(&self) -> Result<()> {
        let path = self.paths.store_schema_version();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let value = fs::read_to_string(&path)?;
            let version =
                value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| StoreError::UnsupportedStoreSchema {
                        path: path.clone(),
                        message: format!(
                            "expected schema_version {STORE_SCHEMA_VERSION}, found {value:?}"
                        ),
                    })?;
            if version != STORE_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedStoreSchema {
                    path,
                    message: format!(
                        "expected schema_version {STORE_SCHEMA_VERSION}, found {version}"
                    ),
                });
            }
            return Ok(());
        }
        fs::write(
            path,
            format!(
                "{STORE_SCHEMA_VERSION}
"
            ),
        )?;
        Ok(())
    }

    pub(crate) fn migrate_legacy_local_dir(&self) -> Result<()> {
        let legacy = self.paths.root().join(GraftPaths::LEGACY_LOCAL_DIR);
        let local = self.paths.local_root();
        if legacy.is_dir() && !local.exists() {
            fs::rename(&legacy, &local)?;
        }
        Ok(())
    }

    pub(crate) fn init_index(&self) -> Result<()> {
        if let Some(parent) = self.paths.index().parent() {
            fs::create_dir_all(parent)?;
        }
        let index_path = self.paths.index();
        if index_path.is_file() {
            let header = fs::read(&index_path).unwrap_or_default();
            if header.len() < 16 || &header[..16] != b"SQLite format 3\0" {
                fs::remove_file(&index_path)?;
            }
        }
        let conn = Connection::open(index_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS patches (
                patch_id TEXT PRIMARY KEY,
                base_state TEXT NOT NULL,
                target_state TEXT NOT NULL,
                admitted_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS patch_constraints (
                patch_id TEXT NOT NULL,
                constraint_id TEXT NOT NULL,
                PRIMARY KEY (patch_id, constraint_id)
            );
            CREATE TABLE IF NOT EXISTS evidence (
                evidence_id TEXT PRIMARY KEY,
                subject TEXT NOT NULL,
                constraint_id TEXT NOT NULL,
                result TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    pub(crate) fn index_patch(&self, patch: &PatchRecord) -> Result<()> {
        let resolved = self.resolve_application(&patch.application)?;
        let conn = Connection::open(self.paths.index())?;
        conn.execute(
            "INSERT OR REPLACE INTO patches (patch_id, base_state, target_state, admitted_at)
             VALUES (?1, ?2, ?3, ?4)",
            (
                patch.id.to_string(),
                serde_json::to_string(&resolved.record.base_state)?,
                serde_json::to_string(&resolved.record.target_state)?,
                patch.provenance.created_at.clone(),
            ),
        )?;
        for plan in constraint_plans(&patch.constraint) {
            conn.execute(
                "INSERT OR REPLACE INTO patch_constraints (patch_id, constraint_id) VALUES (?1, ?2)",
                (patch.id.to_string(), serde_json::to_string(&plan)?),
            )?;
        }
        Ok(())
    }

    pub(crate) fn index_evidence(&self, evidence: &EvidenceRecord) -> Result<()> {
        let conn = Connection::open(self.paths.index())?;
        conn.execute(
            "INSERT OR REPLACE INTO evidence (evidence_id, subject, constraint_id, result)
             VALUES (?1, ?2, ?3, ?4)",
            (
                evidence.id.to_string(),
                evidence.subject.clone(),
                serde_json::to_string(&evidence.plan)?,
                serde_json::to_string(&evidence.result)?,
            ),
        )?;
        Ok(())
    }
}
