use super::*;

impl GraftStore {
    pub fn capture_worktree_snapshot(&self, worktree: impl AsRef<Path>) -> Result<TreeSnapshot> {
        let worktree = worktree.as_ref();
        fs::create_dir_all(self.paths.object_blobs())?;
        let mut entries = Vec::new();
        collect_tree_entries(worktree, worktree, &self.paths.object_blobs(), &mut entries)?;
        Ok(TreeSnapshot::new(entries))
    }

    pub fn capture_target_snapshot(
        &self,
        base: &TreeSnapshot,
        captured: &TreeSnapshot,
    ) -> TreeSnapshot {
        let mut entries = BTreeMap::new();
        for entry in &base.entries {
            if should_skip_snapshot_path(&entry.path) {
                entries.insert(entry.path.clone(), entry.clone());
            }
        }
        for entry in &captured.entries {
            if !should_skip_snapshot_path(&entry.path) {
                entries.insert(entry.path.clone(), entry.clone());
            }
        }
        TreeSnapshot::new(entries.into_values().collect())
    }

    pub fn restore_worktree_paths(
        &self,
        snapshot: &TreeSnapshot,
        worktree: impl AsRef<Path>,
        paths: &[String],
    ) -> Result<()> {
        let worktree = worktree.as_ref();
        for path in paths {
            let destination = materialized_path(worktree, path)?;
            match snapshot.entries.iter().find(|entry| entry.path == *path) {
                Some(entry) => {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    remove_path_if_exists(&destination)?;
                    fs::write(&destination, self.read_blob(&entry.hash)?)?;
                }
                None => {
                    remove_path_if_exists(&destination)?;
                }
            }
        }
        Ok(())
    }

    pub fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        fs::create_dir_all(self.paths.object_blobs())?;
        let hash = blake3::hash(bytes).to_hex().to_string();
        let path = self.paths.object_blobs().join(&hash);
        if !path.exists() {
            fs::write(path, bytes)?;
        }
        Ok(hash)
    }

    pub fn write_blob_object(&self, hash: &str, bytes: &[u8]) -> Result<PathBuf> {
        let actual = blake3::hash(bytes).to_hex().to_string();
        if actual != hash {
            return Err(StoreError::BlobHashMismatch {
                expected: hash.to_string(),
                actual,
            });
        }
        fs::create_dir_all(self.paths.object_blobs())?;
        let path = self.paths.object_blobs().join(hash);
        if !path.exists() {
            fs::write(&path, bytes)?;
        }
        Ok(path)
    }

    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.paths.object_blobs().join(hash))?)
    }

    pub fn list_blob_objects(&self) -> Result<Vec<(String, Vec<u8>)>> {
        if !self.paths.object_blobs().exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(self.paths.object_blobs())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                paths.push(entry.path());
            }
        }
        paths.sort();
        let mut blobs = Vec::new();
        for path in paths {
            let expected = store_object_file_name(&path, "blob")?;
            let bytes = fs::read(&path)?;
            let actual = blake3::hash(&bytes).to_hex().to_string();
            if actual != expected {
                return Err(StoreError::StoreObjectIdMismatch {
                    path,
                    expected,
                    actual,
                });
            }
            blobs.push((expected, bytes));
        }
        Ok(blobs)
    }

    pub fn write_tree_snapshot(&self, snapshot: &TreeSnapshot) -> Result<(String, PathBuf)> {
        fs::create_dir_all(self.paths.object_trees())?;
        let id = snapshot.id().map_err(StoreError::Core)?;
        let path = self.paths.object_trees().join(format!("{id}.json"));
        write_json(&path, snapshot)?;
        Ok((id, path))
    }

    pub fn read_tree_snapshot(&self, id: &str) -> Result<TreeSnapshot> {
        read_json(&self.paths.object_trees().join(format!("{id}.json")))
    }

    pub fn list_tree_objects(&self) -> Result<Vec<(String, TreeSnapshot)>> {
        read_named_json_records::<TreeSnapshot>(&self.paths.object_trees())?
            .into_iter()
            .map(|record| {
                let actual = record.value.id().map_err(StoreError::Core)?;
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn write_action(&self, action: &Action) -> Result<(ActionId, PathBuf)> {
        fs::create_dir_all(self.paths.object_actions())?;
        let id = action_id(action).map_err(StoreError::Core)?;
        let path = self.paths.object_actions().join(format!("{id}.json"));
        write_json(&path, action)?;
        Ok((id, path))
    }

    pub fn read_action(&self, id: &str) -> Result<Action> {
        read_json(&self.paths.object_actions().join(format!("{id}.json")))
    }

    pub fn write_application(
        &self,
        record: &ApplicationRecord,
    ) -> Result<(ApplicationId, PathBuf)> {
        fs::create_dir_all(self.paths.object_applications())?;
        let id = record.id().map_err(StoreError::Core)?;
        let path = self.paths.object_applications().join(format!("{id}.json"));
        write_json(&path, record)?;
        Ok((id, path))
    }

    pub fn read_application(&self, id: &str) -> Result<ApplicationRecord> {
        read_json(&self.paths.object_applications().join(format!("{id}.json")))
    }

    pub fn write_change(&self, change: &Change) -> Result<(graft_core::ChangeId, PathBuf)> {
        fs::create_dir_all(self.paths.object_changes())?;
        let id = change.id().map_err(StoreError::Core)?;
        let path = self.paths.object_changes().join(format!("{id}.json"));
        write_json(&path, change)?;
        Ok((id, path))
    }

    pub fn read_change(&self, id: &str) -> Result<Change> {
        read_json(&self.paths.object_changes().join(format!("{id}.json")))
    }

    pub fn write_materialized_application(
        &self,
        materialized: &MaterializedApplication,
    ) -> Result<ApplicationRef> {
        self.write_action(&materialized.action)?;
        self.write_change(&materialized.change)?;
        let (application_id, _) = self.write_application(&materialized.record)?;
        Ok(ApplicationRef::Stored(application_id))
    }

    pub fn resolve_application(&self, application: &ApplicationRef) -> Result<ResolvedApplication> {
        let ApplicationRef::Stored(expected_application_id) = application;
        let application_path = self
            .paths
            .object_applications()
            .join(format!("{expected_application_id}.json"));
        let record = self.read_application(expected_application_id.as_str())?;
        let actual_application_id = application_id(&record)
            .map_err(StoreError::Core)?
            .to_string();
        if actual_application_id != expected_application_id.as_str() {
            return Err(StoreError::StoreObjectIdMismatch {
                path: application_path,
                expected: expected_application_id.to_string(),
                actual: actual_application_id,
            });
        }
        let change = self.read_change(record.change.as_str())?;
        let action = self.read_action(record.action.as_str())?;
        validate_application_integrity(&record, &action, &change)
            .map_err(graft_core::CoreError::from)?;
        Ok(ResolvedApplication {
            record,
            change,
            action,
        })
    }

    pub fn list_change_objects(&self) -> Result<Vec<(String, Change)>> {
        read_named_json_records::<Change>(&self.paths.object_changes())?
            .into_iter()
            .map(|record| {
                let actual = record.value.id().map_err(StoreError::Core)?.to_string();
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn list_action_objects(&self) -> Result<Vec<(String, Action)>> {
        read_named_json_records::<Action>(&self.paths.object_actions())?
            .into_iter()
            .map(|record| {
                let actual = action_id(&record.value)
                    .map_err(StoreError::Core)?
                    .to_string();
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn list_application_objects(&self) -> Result<Vec<(String, ApplicationRecord)>> {
        read_named_json_records::<ApplicationRecord>(&self.paths.object_applications())?
            .into_iter()
            .map(|record| {
                let actual = record.value.id().map_err(StoreError::Core)?.to_string();
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                self.resolve_application(&ApplicationRef::Stored(ApplicationId::new(
                    record.id.clone(),
                )))?;
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn write_plan(&self, plan: &Plan) -> Result<(PlanId, PathBuf)> {
        fs::create_dir_all(self.paths.object_plans())?;
        let id = plan.plan_id().map_err(StoreError::Core)?;
        let path = self.paths.object_plans().join(format!("{id}.json"));
        write_json(&path, plan)?;
        Ok((id, path))
    }

    pub fn read_plan(&self, id: &str) -> Result<Plan> {
        read_json(&self.paths.object_plans().join(format!("{id}.json")))
    }

    pub fn write_constraint_def(&self, def: &ConstraintDef) -> Result<(String, PathBuf)> {
        fs::create_dir_all(self.paths.object_constraints())?;
        let id = def.body_id().map_err(StoreError::Core)?;
        let path = self.paths.object_constraints().join(format!("{id}.json"));
        write_json(&path, def)?;
        Ok((id, path))
    }

    pub fn read_constraint_def(&self, id: &str) -> Result<ConstraintDef> {
        read_json(&self.paths.object_constraints().join(format!("{id}.json")))
    }
}
