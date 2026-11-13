//! The client that implements [`crate::icloud_api::ICloudClient`] on top of
//! the protocol implementation.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::icloud::auth::{AppleAuth, SessionData, SignInOutcome};
use crate::icloud::drive::{DriveNode, DriveService, DEFAULT_ZONE};
use crate::icloud::transport::{CookieJar, HttpTransport, Transport};
use crate::icloud_api::{ICloudClient, RemoteKind, RemoteNode};

pub struct ICloudApi {
    transport: Arc<dyn Transport>,
    jar: Arc<Mutex<CookieJar>>,
    auth: Mutex<AppleAuth>,
    drive: Mutex<Option<Arc<DriveService>>>,
    authenticated: AtomicBool,
    username: String,
    password: String,
    cookie_dir: PathBuf,
}

impl ICloudApi {
    /// Build a client from the configuration. The session is restored from
    /// `cookie_dir` when there is one.
    pub fn new(
        username: &str,
        password: &str,
        cookie_dir: &Path,
        transport: Option<Arc<dyn Transport>>,
    ) -> Result<Self> {
        let transport: Arc<dyn Transport> = match transport {
            Some(transport) => transport,
            None => Arc::new(HttpTransport::new(crate::icloud::auth::USER_AGENT)?),
        };
        let (session, jar) = AppleAuth::load(cookie_dir, username).unwrap_or_default();
        let jar = Arc::new(Mutex::new(jar));
        let auth = AppleAuth::new(
            transport.clone(),
            jar.clone(),
            username,
            password,
            session,
            Some(crate::icloud::auth::session_path(cookie_dir, username)),
        );
        Ok(Self {
            transport,
            jar,
            auth: Mutex::new(auth),
            drive: Mutex::new(None),
            authenticated: AtomicBool::new(false),
            username: username.to_string(),
            password: password.to_string(),
            cookie_dir: cookie_dir.to_path_buf(),
        })
    }

    /// The stored session.
    pub fn session(&self) -> SessionData {
        self.auth
            .lock()
            .map(|auth| auth.session().clone())
            .unwrap_or_default()
    }

    /// The drive service, built from the endpoints of the account.
    fn service(&self) -> Result<Arc<DriveService>> {
        if let Some(service) = self.drive.lock().ok().and_then(|d| d.clone()) {
            return Ok(service);
        }
        let session = self.session();
        let drive_url = session
            .webservices
            .drivews
            .clone()
            .ok_or_else(|| anyhow!("the account description has no drive endpoint"))?;
        let doc_url = session
            .webservices
            .docws
            .clone()
            .ok_or_else(|| anyhow!("the account description has no document endpoint"))?;
        let service = Arc::new(DriveService::new(
            Box::new(TransportBox(self.transport.clone())),
            drive_url,
            doc_url,
            session.client_id.clone(),
        ));
        if let Ok(mut guard) = self.drive.lock() {
            *guard = Some(service.clone());
        }
        Ok(service)
    }

    /// Restore a stored session and check whether it is still valid.
    pub fn restore(&self) -> Result<()> {
        let mut auth = self
            .auth
            .lock()
            .map_err(|_| anyhow!("the session is poisoned"))?;
        if auth.session().session_token.is_none() {
            return Err(anyhow!("no stored session"));
        }
        auth.validate()?;
        drop(auth);
        self.authenticated.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Sign in, asking for a verification code when the service demands one.
    ///
    /// `code_provider` is called when a code is needed; it returns the code
    /// the user typed.
    pub fn signin_interactive<F>(&self, mut code_provider: F) -> Result<()>
    where
        F: FnMut(&SignInOutcome) -> Result<String>,
    {
        let mut auth = self
            .auth
            .lock()
            .map_err(|_| anyhow!("the session is poisoned"))?;
        let _ = auth.detect_partition();
        let outcome = auth.signin()?;
        if let SignInOutcome::TwoFactorRequired { .. } = outcome {
            // Ask for the code the user received.
            let code = code_provider(&outcome)?;
            auth.submit_code(&code)?;
        }
        self.authenticated.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// The trusted phone numbers of the account.
    pub fn trusted_phone_numbers(&self) -> Result<Vec<crate::icloud::auth::TrustedPhone>> {
        let auth = self
            .auth
            .lock()
            .map_err(|_| anyhow!("the session is poisoned"))?;
        Ok(auth.trusted_phone_numbers()?)
    }

    /// Ask for a code by SMS.
    pub fn request_sms_code(&self, phone_id: i64) -> Result<()> {
        let mut auth = self
            .auth
            .lock()
            .map_err(|_| anyhow!("the session is poisoned"))?;
        Ok(auth.request_sms_code(phone_id)?)
    }

    /// Submit a code that was sent by SMS.
    pub fn submit_sms_code(&self, code: &str, phone_id: i64) -> Result<()> {
        let mut auth = self
            .auth
            .lock()
            .map_err(|_| anyhow!("the session is poisoned"))?;
        Ok(auth.submit_sms_code(code, phone_id)?)
    }

    /// Ask the service to push a code to the trusted devices.
    pub fn request_push(&self) -> Result<()> {
        let mut auth = self
            .auth
            .lock()
            .map_err(|_| anyhow!("the session is poisoned"))?;
        Ok(auth.request_push()?)
    }

    /// The account name and where the session is stored.
    pub fn account(&self) -> (&str, &Path) {
        (&self.username, &self.cookie_dir)
    }

    /// Resolve a path to a node of the drive.
    fn resolve(&self, path: &str) -> Result<Option<DriveNode>> {
        let service = self.service()?;
        let path = crate::config::normalize_icloud_path(path);
        Ok(service.lookup_path(&path)?)
    }
}

impl ICloudClient for ICloudApi {
    fn authenticate(&mut self, _interactive: bool) -> Result<()> {
        // The interactive code entry belongs to the `icloudctl auth` command;
        // here the stored session is simply validated.
        self.restore().map_err(|_| {
            anyhow!("the stored session is not valid; run `icloudctl auth` to sign in again")
        })
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::SeqCst)
    }

    fn stat_node(&self, path: &str) -> Result<Option<RemoteNode>> {
        let node = self.resolve(path)?;
        Ok(node.as_ref().map(to_remote))
    }

    fn list_children(&self, path: &str) -> Result<Vec<RemoteNode>> {
        let service = self.service()?;
        let Some(node) = self.resolve(path)? else {
            return Ok(Vec::new());
        };
        let drivewsid = node
            .drivewsid
            .clone()
            .ok_or_else(|| anyhow!("the folder has no drive identifier"))?;
        let children = service.list_children(&drivewsid)?;
        Ok(children.iter().map(to_remote).collect())
    }

    fn download_file(&self, node: &RemoteNode, dest: &Path) -> Result<u64> {
        let service = self.service()?;
        Ok(service.download(&from_remote(node), dest)?)
    }

    fn upload_file(
        &self,
        local_path: &Path,
        remote_parent: &str,
        name: &str,
    ) -> Result<RemoteNode> {
        let service = self.service()?;
        let Some(parent) = self.resolve(remote_parent)? else {
            return Err(anyhow!(
                "the target folder does not exist: {}",
                remote_parent
            ));
        };
        let uploaded = service.upload(&parent, local_path, name)?;
        Ok(to_remote(&uploaded))
    }

    fn mkdir(&self, remote_parent: &str, name: &str) -> Result<RemoteNode> {
        let service = self.service()?;
        let Some(parent) = self.resolve(remote_parent)? else {
            return Err(anyhow!(
                "the parent folder does not exist: {}",
                remote_parent
            ));
        };
        let created = service.create_folder(&parent, name)?;
        Ok(to_remote(&created))
    }

    fn delete(&self, remote_id: &str) -> Result<()> {
        let service = self.service()?;
        let Some(node) = service.item_details(remote_id)? else {
            return Err(anyhow!("the node does not exist: {}", remote_id));
        };
        Ok(service.delete(&node)?)
    }

    fn rename_move(
        &self,
        source_path: &str,
        remote_parent: &str,
        name: &str,
    ) -> Result<RemoteNode> {
        let service = self.service()?;
        let Some(source) = self.resolve(source_path)? else {
            return Err(anyhow!("the source does not exist: {}", source_path));
        };
        let Some(parent) = self.resolve(remote_parent)? else {
            return Err(anyhow!(
                "the target folder does not exist: {}",
                remote_parent
            ));
        };

        let same_parent = source.parent_id == parent.drivewsid;
        let mut node = source;
        if !same_parent {
            node = service.move_to(&node, &parent)?;
        }
        if !name.is_empty() && node.name != name {
            node = service.rename(&node, name)?;
        }
        Ok(to_remote(&node))
    }
}

/// A thin wrapper so the transport can be shared behind a `Box`.
struct TransportBox(Arc<dyn Transport>);

impl Transport for TransportBox {
    fn execute(
        &self,
        request: &crate::icloud::transport::HttpRequest,
    ) -> crate::icloud::error::Result<crate::icloud::transport::HttpResponse> {
        self.0.execute(request)
    }
}

/// Convert a protocol node into the node type of the client interface.
fn to_remote(node: &DriveNode) -> RemoteNode {
    RemoteNode {
        path: node.name.clone(),
        kind: if node.is_folder() {
            RemoteKind::Folder
        } else {
            RemoteKind::File
        },
        remote_id: node.drivewsid.clone(),
        docwsid: node.docwsid.clone(),
        zone: Some(
            node.zone
                .clone()
                .unwrap_or_else(|| DEFAULT_ZONE.to_string()),
        ),
        etag: node.etag.clone(),
        size: node.size,
        mtime: node.mtime,
    }
}

/// Convert a client node back into a protocol node.
fn from_remote(node: &RemoteNode) -> DriveNode {
    DriveNode {
        drivewsid: node.remote_id.clone(),
        docwsid: node.docwsid.clone(),
        name: node.path.clone(),
        node_type: match node.kind {
            RemoteKind::Folder => "folder".to_string(),
            RemoteKind::File => "file".to_string(),
        },
        etag: node.etag.clone(),
        zone: node.zone.clone(),
        size: node.size,
        mtime: node.mtime,
        parent_id: None,
    }
}
