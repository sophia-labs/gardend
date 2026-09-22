use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    paths::{graphs_dir, profile_dir},
    runtime_config::{PROFILE_ID, RUNTIME_PROFILE},
    storage::{create_dir_all, display_path, read_json, write_json},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileManifest {
    pub(crate) profile_id: String,
    pub(crate) display_name: String,
    pub(crate) runtime_profile: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileInfo {
    profile_id: String,
    display_name: String,
    runtime_profile: String,
    profile_path: String,
    graphs_path: String,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_profile(app: AppHandle) -> Result<ProfileInfo, String> {
    let profile = ensure_profile(&app)?;
    let profile_dir = profile_dir(&app)?;
    let graphs_dir = graphs_dir(&app)?;

    Ok(ProfileInfo {
        profile_id: profile.profile_id,
        display_name: profile.display_name,
        runtime_profile: profile.runtime_profile,
        profile_path: display_path(&profile_dir),
        graphs_path: display_path(&graphs_dir),
    })
}

pub(crate) fn ensure_profile(app: &AppHandle) -> Result<ProfileManifest, String> {
    let profile_dir = profile_dir(app)?;
    let graphs_dir = profile_dir.join("graphs");
    create_dir_all(&graphs_dir)?;

    let profile_path = profile_dir.join("profile.json");
    let profile = if profile_path.is_file() {
        read_json::<ProfileManifest>(&profile_path)?
    } else {
        let now = timestamp();
        let profile = ProfileManifest {
            profile_id: PROFILE_ID.to_string(),
            display_name: "Default Local Profile".to_string(),
            runtime_profile: RUNTIME_PROFILE.to_string(),
            created_at: now.clone(),
            updated_at: now,
        };

        write_json(&profile_path, &profile)?;
        write_json(
            &profile_dir.join("identity.json"),
            &serde_json::json!({
              "identityKind": "local_profile",
              "profileId": PROFILE_ID,
              "hostedAccount": null,
              "createdAt": profile.created_at,
            }),
        )?;
        profile
    };

    // A remote embeddings pool is the cell's semantic authority and supplies
    // its own model identity. Creating or validating a local Omphalos in that
    // mode would reintroduce the constitution dependency the remote path is
    // specifically meant to remove.
    if crate::semantic_model_remote::remote_embeddings_endpoint().is_none() {
        crate::omphalos::initialize_constitution(app)?;
    }
    Ok(profile)
}

pub(crate) fn touch_profile_updated_at(app: &AppHandle) -> Result<(), String> {
    let profile_path = profile_dir(app)?.join("profile.json");
    let mut profile = read_json::<ProfileManifest>(&profile_path)?;
    profile.updated_at = timestamp();
    write_json(&profile_path, &profile).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_manifest_uses_camel_case_json_fields() {
        let profile = ProfileManifest {
            profile_id: "default".to_string(),
            display_name: "Default Local Profile".to_string(),
            runtime_profile: "local_only".to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
        };

        let value = serde_json::to_value(profile).expect("serialize profile");

        assert_eq!(value["profileId"], "default");
        assert_eq!(value["runtimeProfile"], "local_only");
        assert!(value.get("profile_id").is_none());
    }
}
