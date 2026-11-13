//! The iCloud Drive service.
//!
//! The endpoints used here are the ones Apple's own web front end uses. Two
//! services take part:
//!
//! * **drivews** — the drive service: creates, renames, moves and deletes
//!   nodes, and answers questions about them.
//! * **docws** — the document service: resolves a path to a node, lists a
//!   folder, hands out download URLs and receives uploads.

use std::path::Path;

use serde_json::{json, Value};

use super::error::{Error, Result};
use super::transport::{HttpRequest, Method, Transport};

/// The drive identifier of the root folder.
pub const ROOT_ID: &str = "FOLDER::com.apple.CloudDocs::root";
/// The zone that holds the user's documents.
pub const DEFAULT_ZONE: &str = "com.apple.CloudDocs";

/// A node of the drive.
#[derive(Debug, Clone)]
pub struct DriveNode {
    pub drivewsid: Option<String>,
    pub docwsid: Option<String>,
    pub name: String,
    pub node_type: String,
    pub etag: Option<String>,
    pub zone: Option<String>,
    pub size: u64,
    pub mtime: i64,
    pub parent_id: Option<String>,
}

impl DriveNode {
    pub fn is_folder(&self) -> bool {
        matches!(
            self.node_type.to_ascii_lowercase().as_str(),
            "folder" | "app_library" | "shared_folder"
        )
    }

    /// Parse a node from the service's representation.
    pub fn from_json(value: &Value) -> DriveNode {
        let name = match (
            value.get("name").and_then(|v| v.as_str()),
            value.get("extension").and_then(|v| v.as_str()),
        ) {
            (Some(name), Some(extension)) if !extension.is_empty() => {
                format!("{}.{}", name, extension)
            }
            (Some(name), _) => name.to_string(),
            _ => String::new(),
        };
        DriveNode {
            drivewsid: value
                .get("drivewsid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            docwsid: value
                .get("docwsid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            name,
            node_type: value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("file")
                .to_string(),
            etag: value
                .get("etag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            zone: value
                .get("zone")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            size: value.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            mtime: parse_time(value.get("dateModified")),
            parent_id: value
                .get("parentId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        }
    }
}

/// Parse an Apple timestamp (ISO 8601 with a zone offset) into a unix time.
fn parse_time(value: Option<&Value>) -> i64 {
    let Some(text) = value.and_then(|v| v.as_str()) else {
        return 0;
    };
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return parsed.timestamp();
    }
    if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S") {
        return parsed.and_utc().timestamp();
    }
    0
}

pub struct DriveService {
    transport: Box<dyn Transport>,
    drivews: String,
    docws: String,
    client_id: String,
}

impl DriveService {
    pub fn new(
        transport: Box<dyn Transport>,
        drivews: String,
        docws: String,
        client_id: String,
    ) -> Self {
        Self {
            transport,
            drivews,
            docws,
            client_id,
        }
    }

    fn params(&self) -> Vec<(String, String)> {
        vec![("clientId".to_string(), self.client_id.clone())]
    }

    fn request(&self, method: Method, url: String, body: Option<Value>) -> Result<Value> {
        let mut request = HttpRequest::new(method, url);
        for (key, value) in self.params() {
            request = request.query(&key, value);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: None,
                message: format!("{} {}", method.as_str(), response.status),
            });
        }
        if response.body.is_empty() {
            return Ok(Value::Null);
        }
        response.json()
    }

    /// Resolve a path to a node.
    ///
    /// This is what makes the lazy index possible: the service resolves the
    /// path, so the client does not have to walk the tree level by level.
    pub fn lookup_path(&self, path: &str) -> Result<Option<DriveNode>> {
        let url = format!("{}/ws/{}/list/lookup_by_path", self.docws, DEFAULT_ZONE);
        let mut request = HttpRequest::new(Method::Post, url)
            .query("unified_format", "true")
            .text(path.to_string());
        for (key, value) in self.params() {
            request = request.query(&key, value);
        }
        let response = self.transport.execute(&request)?;
        if response.status == 404 || response.status == 204 {
            return Ok(None);
        }
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: None,
                message: format!("path lookup failed for {}", path),
            });
        }
        let value = response.json()?;
        let items = value.as_array().cloned().unwrap_or_default();
        Ok(items.first().map(DriveNode::from_json))
    }

    /// The children of a folder.
    pub fn list_children(&self, drivewsid: &str) -> Result<Vec<DriveNode>> {
        let url = format!("{}/v1/enumerate/{}", self.docws, drivewsid);
        let mut request = HttpRequest::new(Method::Get, url).query("limit", "1000");
        for (key, value) in self.params() {
            request = request.query(&key, value);
        }
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: None,
                message: "listing failed".to_string(),
            });
        }
        let value = response.json()?;
        let items = value
            .get("drive_item")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().map(DriveNode::from_json).collect())
    }

    /// The details of a node, without its children.
    pub fn item_details(&self, drivewsid: &str) -> Result<Option<DriveNode>> {
        let body = json!({
            "items": [{
                "drivewsid": drivewsid,
                "partialData": false,
                "includeHierarchy": false,
            }]
        });
        let value = self.request(
            Method::Post,
            format!("{}/retrieveItemDetails", self.drivews),
            Some(body),
        )?;
        let items = value
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .or_else(|| value.as_array().cloned())
            .unwrap_or_default();
        Ok(items.first().map(DriveNode::from_json))
    }

    /// The URL the content of a file can be downloaded from.
    pub fn download_url(&self, node: &DriveNode) -> Result<String> {
        let docwsid = node
            .docwsid
            .as_ref()
            .ok_or_else(|| Error::Protocol("the node has no document identifier".into()))?;
        let zone = node.zone.as_deref().unwrap_or(DEFAULT_ZONE);
        let mut request = HttpRequest::new(
            Method::Get,
            format!("{}/ws/{}/download/by_id", self.docws, zone),
        )
        .query("document_id", docwsid.clone());
        for (key, value) in self.params() {
            request = request.query(&key, value);
        }
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: None,
                message: "could not obtain a download url".to_string(),
            });
        }
        let value = response.json()?;
        for key in ["data_token", "package_token"] {
            if let Some(url) = value
                .get(key)
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str())
            {
                return Ok(url.to_string());
            }
        }
        Err(Error::Protocol(
            "the service returned neither a data nor a package token".into(),
        ))
    }

    /// Download a file to `dest`.
    pub fn download(&self, node: &DriveNode, dest: &Path) -> Result<u64> {
        let url = self.download_url(node)?;
        let request = HttpRequest::new(Method::Get, url);
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: None,
                message: "download failed".to_string(),
            });
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dest, &response.body)?;
        Ok(response.body.len() as u64)
    }

    /// Upload a file into a folder.
    ///
    /// The service expects three steps: ask for an upload slot, send the
    /// content, and finally register the document in the target folder.
    pub fn upload(&self, parent: &DriveNode, file: &Path, name: &str) -> Result<DriveNode> {
        let size = std::fs::metadata(file)?.len();
        let zone = parent.zone.as_deref().unwrap_or(DEFAULT_ZONE);

        // 1. Ask for an upload slot.
        let body = json!({
            "filename": name,
            "type": "FILE",
            "size": size.to_string(),
            "content_type": content_type_for(name),
        });
        let slot = self.request(
            Method::Post,
            format!("{}/ws/{}/upload/web", self.docws, zone),
            Some(body),
        )?;
        let slot = slot
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .ok_or_else(|| Error::Protocol("the service returned no upload slot".into()))?;
        let document_id = slot
            .get("document_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Protocol("the upload slot has no document id".into()))?
            .to_string();
        let upload_url = slot
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Protocol("the upload slot has no url".into()))?
            .to_string();

        // 2. Send the content.
        let request =
            HttpRequest::new(Method::Post, upload_url).body(super::transport::Body::File {
                path: file.to_path_buf(),
                file_name: name.to_string(),
                content_type: content_type_for(name).to_string(),
            });
        let response = self.transport.execute(&request)?;
        if response.status >= 400 {
            return Err(Error::Api {
                status: response.status,
                code: None,
                message: "the upload was rejected".to_string(),
            });
        }
        let uploaded = response.json()?;
        let single = uploaded
            .get("singleFile")
            .ok_or_else(|| Error::Protocol("the upload returned no file receipt".into()))?;

        // 3. Register the document in the folder.
        let mut data = json!({
            "signature": single.get("fileChecksum").and_then(|v| v.as_str()).unwrap_or_default(),
            "wrapping_key": single.get("wrappingKey").and_then(|v| v.as_str()).unwrap_or_default(),
            "reference_signature": single.get("referenceChecksum").and_then(|v| v.as_str()).unwrap_or_default(),
            "size": single.get("size").and_then(|v| v.as_u64()).unwrap_or(size),
        });
        if let Some(receipt) = single.get("receipt").and_then(|v| v.as_str()) {
            data["receipt"] = json!(receipt);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let body = json!({
            "data": data,
            "command": "add_file",
            "create_short_guid": true,
            "document_id": document_id,
            "path": {
                "starting_document_id": parent.docwsid.clone().unwrap_or_default(),
                "path": name,
            },
            "allow_conflict": true,
            "file_flags": {
                "is_writable": true,
                "is_executable": false,
                "is_hidden": false,
            },
            "mtime": now,
            "btime": now,
        });
        let value = self.request(
            Method::Post,
            format!("{}/ws/{}/update/documents", self.docws, zone),
            Some(body),
        )?;
        let document = value
            .get("results")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("document"))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(DriveNode::from_json(&document))
    }

    /// Create a folder below `parent`.
    pub fn create_folder(&self, parent: &DriveNode, name: &str) -> Result<DriveNode> {
        let body = json!({
            "destinationDrivewsId": parent.drivewsid.clone().unwrap_or_default(),
            "folders": [{
                "clientId": format!("FOLDER::UNKNOWN_ZONE::TempId-{}", self.client_id),
                "name": name,
            }],
        });
        let value = self.request(
            Method::Post,
            format!("{}/createFolders", self.drivews),
            Some(body),
        )?;
        let folder = value
            .get("folders")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        Ok(DriveNode::from_json(&folder))
    }

    /// Rename a node.
    pub fn rename(&self, node: &DriveNode, name: &str) -> Result<DriveNode> {
        let body = json!({
            "items": [{
                "drivewsid": node.drivewsid.clone().unwrap_or_default(),
                "etag": node.etag.clone().unwrap_or_default(),
                "name": name,
            }],
        });
        let value = self.request(
            Method::Post,
            format!("{}/renameItems", self.drivews),
            Some(body),
        )?;
        let item = value
            .get("items")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        Ok(DriveNode::from_json(&item))
    }

    /// Move a node below another folder.
    pub fn move_to(&self, node: &DriveNode, destination: &DriveNode) -> Result<DriveNode> {
        let body = json!({
            "destinationDrivewsId": destination.drivewsid.clone().unwrap_or_default(),
            "items": [{
                "drivewsid": node.drivewsid.clone().unwrap_or_default(),
                "etag": node.etag.clone().unwrap_or_default(),
                "clientId": node.drivewsid.clone().unwrap_or_default(),
            }],
        });
        let value = self.request(
            Method::Post,
            format!("{}/moveItems", self.drivews),
            Some(body),
        )?;
        let item = value
            .get("items")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        Ok(DriveNode::from_json(&item))
    }

    /// Move a node to the trash.
    pub fn delete(&self, node: &DriveNode) -> Result<()> {
        let body = json!({
            "items": [{
                "drivewsid": node.drivewsid.clone().unwrap_or_default(),
                "etag": node.etag.clone().unwrap_or_default(),
                "clientId": node.drivewsid.clone().unwrap_or_default(),
            }],
        });
        self.request(
            Method::Post,
            format!("{}/moveItemsToTrash", self.drivews),
            Some(body),
        )?;
        Ok(())
    }
}

/// Guess the content type of a file name.
pub fn content_type_for(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let extension = lower.rsplit('.').next().unwrap_or("");
    match extension {
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "heic" => "image/heic",
        _ => "application/octet-stream",
    }
}
