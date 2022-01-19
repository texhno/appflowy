mod migration;
pub mod version_1;
mod version_2;

use flowy_collaboration::{
    entities::revision::{Revision, RevisionState},
    folder::FolderPad,
};
use parking_lot::RwLock;
use std::sync::Arc;
pub use version_1::{app_sql::*, trash_sql::*, v1_impl::V1Transaction, view_sql::*, workspace_sql::*};

use crate::{
    module::{WorkspaceDatabase, WorkspaceUser},
    services::persistence::{migration::FolderMigration, version_2::v2_impl::FolderEditor},
};
use flowy_core_data_model::entities::{
    app::App,
    prelude::RepeatedTrash,
    trash::Trash,
    view::View,
    workspace::Workspace,
};
use flowy_error::{FlowyError, FlowyResult};
use flowy_sync::{mk_revision_disk_cache, RevisionCache, RevisionManager, RevisionRecord};

pub const FOLDER_ID: &str = "flowy_folder";

pub trait FolderPersistenceTransaction {
    fn create_workspace(&self, user_id: &str, workspace: Workspace) -> FlowyResult<()>;
    fn read_workspaces(&self, user_id: &str, workspace_id: Option<String>) -> FlowyResult<Vec<Workspace>>;
    fn update_workspace(&self, changeset: WorkspaceChangeset) -> FlowyResult<()>;
    fn delete_workspace(&self, workspace_id: &str) -> FlowyResult<()>;

    fn create_app(&self, app: App) -> FlowyResult<()>;
    fn update_app(&self, changeset: AppChangeset) -> FlowyResult<()>;
    fn read_app(&self, app_id: &str) -> FlowyResult<App>;
    fn read_workspace_apps(&self, workspace_id: &str) -> FlowyResult<Vec<App>>;
    fn delete_app(&self, app_id: &str) -> FlowyResult<App>;

    fn create_view(&self, view: View) -> FlowyResult<()>;
    fn read_view(&self, view_id: &str) -> FlowyResult<View>;
    fn read_views(&self, belong_to_id: &str) -> FlowyResult<Vec<View>>;
    fn update_view(&self, changeset: ViewChangeset) -> FlowyResult<()>;
    fn delete_view(&self, view_id: &str) -> FlowyResult<()>;

    fn create_trash(&self, trashes: Vec<Trash>) -> FlowyResult<()>;
    fn read_trash(&self, trash_id: Option<String>) -> FlowyResult<RepeatedTrash>;
    fn delete_trash(&self, trash_ids: Option<Vec<String>>) -> FlowyResult<()>;
}

pub struct FolderPersistence {
    user: Arc<dyn WorkspaceUser>,
    database: Arc<dyn WorkspaceDatabase>,
    folder_editor: RwLock<Option<Arc<FolderEditor>>>,
}

impl FolderPersistence {
    pub fn new(user: Arc<dyn WorkspaceUser>, database: Arc<dyn WorkspaceDatabase>) -> Self {
        let folder_editor = RwLock::new(None);
        Self {
            user,
            database,
            folder_editor,
        }
    }

    #[deprecated(
        since = "0.0.3",
        note = "please use `begin_transaction` instead, this interface will be removed in the future"
    )]
    #[allow(dead_code)]
    pub fn begin_transaction_v_1<F, O>(&self, f: F) -> FlowyResult<O>
    where
        F: for<'a> FnOnce(Box<dyn FolderPersistenceTransaction + 'a>) -> FlowyResult<O>,
    {
        //[[immediate_transaction]]
        // https://sqlite.org/lang_transaction.html
        // IMMEDIATE cause the database connection to start a new write immediately,
        // without waiting for a write statement. The BEGIN IMMEDIATE might fail
        // with SQLITE_BUSY if another write transaction is already active on another
        // database connection.
        //
        // EXCLUSIVE is similar to IMMEDIATE in that a write transaction is started
        // immediately. EXCLUSIVE and IMMEDIATE are the same in WAL mode, but in
        // other journaling modes, EXCLUSIVE prevents other database connections from
        // reading the database while the transaction is underway.
        let conn = self.database.db_connection()?;
        conn.immediate_transaction::<_, FlowyError, _>(|| f(Box::new(V1Transaction(&conn))))
    }

    pub fn begin_transaction<F, O>(&self, f: F) -> FlowyResult<O>
    where
        F: FnOnce(Arc<dyn FolderPersistenceTransaction>) -> FlowyResult<O>,
    {
        match self.folder_editor.read().clone() {
            None => {
                tracing::error!("FolderEditor should be initialized after user login in.");
                let editor = futures::executor::block_on(async { self.init_folder_editor().await })?;
                f(editor)
            },
            Some(editor) => f(editor),
        }
    }

    pub fn user_did_logout(&self) { *self.folder_editor.write() = None; }

    pub async fn initialize(&self, user_id: &str) -> FlowyResult<()> {
        let migrations = FolderMigration::new(user_id, self.database.clone());
        if let Some(migrated_folder) = migrations.run_v1_migration()? {
            tracing::trace!("Save migration folder");
            self.save_folder(user_id, migrated_folder).await?;
        }

        let _ = self.init_folder_editor().await?;
        Ok(())
    }

    async fn init_folder_editor(&self) -> FlowyResult<Arc<FolderEditor>> {
        let user_id = self.user.user_id()?;
        let token = self.user.token()?;
        let pool = self.database.db_pool()?;
        let folder_editor = FolderEditor::new(&user_id, &token, pool).await?;
        let editor = Arc::new(folder_editor);
        *self.folder_editor.write() = Some(editor.clone());
        Ok(editor)
    }

    pub async fn save_folder(&self, user_id: &str, folder: FolderPad) -> FlowyResult<()> {
        let pool = self.database.db_pool()?;
        let delta_data = folder.delta().to_bytes();
        let md5 = folder.md5();
        let revision = Revision::new(FOLDER_ID, 0, 0, delta_data, user_id, md5);
        let record = RevisionRecord {
            revision,
            state: RevisionState::Sync,
            write_to_disk: true,
        };

        let conn = pool.get()?;
        let disk_cache = mk_revision_disk_cache(user_id, pool);
        disk_cache.write_revision_records(vec![record], &conn)
    }
}