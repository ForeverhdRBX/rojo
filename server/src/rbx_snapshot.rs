use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    str,
};

use serde_derive::{Serialize, Deserialize};
use maplit::hashmap;
use rbx_tree::{RbxTree, RbxValue, RbxInstanceProperties};
use failure::Fail;

use crate::{
    imfs::{
        Imfs,
        ImfsItem,
        ImfsFile,
        ImfsDirectory,
    },
    project::{
        Project,
        ProjectNode,
        InstanceProjectNode,
        SyncPointProjectNode,
    },
    snapshot_reconciler::{
        RbxSnapshotInstance,
        snapshot_from_tree,
    },
};

const INIT_MODULE_NAME: &str = "init.lua";
const INIT_SERVER_NAME: &str = "init.server.lua";
const INIT_CLIENT_NAME: &str = "init.client.lua";

pub type SnapshotResult<'a> = Result<Option<RbxSnapshotInstance<'a>>, SnapshotError>;

pub struct SnapshotMetadata<'meta> {
    pub sync_point_names: &'meta mut HashMap<PathBuf, String>,
}

#[derive(Debug, Fail)]
pub enum SnapshotError {
    DidNotExist(PathBuf),

    Utf8Error {
        #[fail(cause)]
        inner: str::Utf8Error,
        path: PathBuf,
    },

    XmlModelDecodeError {
        inner: rbx_xml::DecodeError,
        path: PathBuf,
    },

    BinaryModelDecodeError {
        inner: rbx_binary::DecodeError,
        path: PathBuf,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, output: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SnapshotError::DidNotExist(path) => write!(output, "Path did not exist: {}", path.display()),
            SnapshotError::Utf8Error { inner, path } => {
                write!(output, "Invalid UTF-8: {} in path {}", inner, path.display())
            },
            SnapshotError::XmlModelDecodeError { inner, path } => {
                write!(output, "Malformed rbxmx model: {:?} in path {}", inner, path.display())
            },
            SnapshotError::BinaryModelDecodeError { inner, path } => {
                write!(output, "Malformed rbxm model: {:?} in path {}", inner, path.display())
            },
        }
    }
}

pub fn snapshot_project_tree<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    project: &'source Project,
) -> SnapshotResult<'source> {
    snapshot_project_node(imfs, metadata, &project.tree, &project.name)
}

fn snapshot_project_node<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    node: &'source ProjectNode,
    instance_name: &'source str,
) -> SnapshotResult<'source> {
    match node {
        ProjectNode::Instance(instance_node) => snapshot_instance_node(imfs, metadata, instance_node, instance_name),
        ProjectNode::SyncPoint(sync_node) => snapshot_sync_point_node(imfs, metadata, sync_node, instance_name),
    }
}

fn snapshot_instance_node<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    node: &'source InstanceProjectNode,
    instance_name: &'source str,
) -> SnapshotResult<'source> {
    let mut children = Vec::new();

    for (child_name, child_project_node) in &node.children {
        if let Some(child) = snapshot_project_node(imfs, metadata, child_project_node, child_name)? {
            children.push(child);
        }
    }

    Ok(Some(RbxSnapshotInstance {
        class_name: Cow::Borrowed(&node.class_name),
        name: Cow::Borrowed(instance_name),
        properties: node.properties.clone(),
        children,
        source_path: None,
        metadata: Some(node.metadata.clone()),
    }))
}

fn snapshot_sync_point_node<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    node: &'source SyncPointProjectNode,
    instance_name: &'source str,
) -> SnapshotResult<'source> {
    // If the snapshot resulted in no instances, like if it targets an unknown
    // file or an empty model file, we can early-return.
    let mut snapshot = match snapshot_imfs_path(imfs, metadata, &node.path)? {
        Some(snapshot) => snapshot,
        None => return Ok(None),
    };

    // Otherwise, we can mutate the snapshot we got back and track some extra
    // metadata.
    snapshot.name = Cow::Borrowed(instance_name);
    metadata.sync_point_names.insert(node.path.to_owned(), instance_name.to_owned());

    Ok(Some(snapshot))
}

pub fn snapshot_imfs_path<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    path: &Path
) -> SnapshotResult<'source> {
    // If the given path doesn't exist in the in-memory filesystem, we consider
    // that an error.
    match imfs.get(path) {
        Some(imfs_item) => snapshot_imfs_item(imfs, metadata, imfs_item),
        None => return Err(SnapshotError::DidNotExist(path.to_owned())),
    }
}

fn snapshot_imfs_item<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    item: &'source ImfsItem,
) -> SnapshotResult<'source> {
    match item {
        ImfsItem::File(file) => snapshot_imfs_file(metadata, file),
        ImfsItem::Directory(directory) => snapshot_imfs_directory(imfs, metadata, directory),
    }
}

fn snapshot_imfs_directory<'source>(
    imfs: &'source Imfs,
    metadata: &mut SnapshotMetadata,
    directory: &'source ImfsDirectory,
) -> SnapshotResult<'source> {
    let init_path = directory.path.join(INIT_MODULE_NAME);
    let init_server_path = directory.path.join(INIT_SERVER_NAME);
    let init_client_path = directory.path.join(INIT_CLIENT_NAME);

    let mut snapshot = if directory.children.contains(&init_path) {
        snapshot_imfs_path(imfs, metadata, &init_path)?.unwrap()
    } else if directory.children.contains(&init_server_path) {
        snapshot_imfs_path(imfs, metadata, &init_server_path)?.unwrap()
    } else if directory.children.contains(&init_client_path) {
        snapshot_imfs_path(imfs, metadata, &init_client_path)?.unwrap()
    } else {
        RbxSnapshotInstance {
            class_name: Cow::Borrowed("Folder"),
            name: Cow::Borrowed(""),
            properties: HashMap::new(),
            children: Vec::new(),
            source_path: Some(directory.path.clone()),
            metadata: None,
        }
    };

    // We have to be careful not to lose instance names that are specified in
    // the project manifest. We store them in sync_point_names when the original
    // tree is constructed.
    snapshot.name = if let Some(actual_name) = metadata.sync_point_names.get(&directory.path) {
        Cow::Owned(actual_name.clone())
    } else {
        Cow::Borrowed(directory.path
            .file_name().expect("Could not extract file name")
            .to_str().expect("Could not convert path to UTF-8"))
    };

    for child_path in &directory.children {
        let child_name = child_path
            .file_name().expect("Couldn't extract file name")
            .to_str().expect("Couldn't convert file name to UTF-8");

        match child_name {
            INIT_MODULE_NAME | INIT_SERVER_NAME | INIT_CLIENT_NAME => {
                // The existence of files with these names modifies the
                // parent instance and is handled above, so we can skip
                // them here.
            },
            _ => {
                if let Some(child) = snapshot_imfs_path(imfs, metadata, child_path)? {
                    snapshot.children.push(child);
                }
            },
        }
    }

    Ok(Some(snapshot))
}

fn snapshot_imfs_file<'source>(
    metadata: &mut SnapshotMetadata,
    file: &'source ImfsFile,
) -> SnapshotResult<'source> {
    let extension = file.path.extension()
        .map(|v| v.to_str().expect("Could not convert extension to UTF-8"));

    let mut maybe_snapshot = match extension {
        Some("lua") => snapshot_lua_file(file)?,
        Some("csv") => snapshot_csv_file(file)?,
        Some("txt") => snapshot_txt_file(file)?,
        Some("rbxmx") => snapshot_xml_model_file(file)?,
        Some("rbxm") => snapshot_binary_model_file(file)?,
        Some(_) | None => return Ok(None),
    };

    if let Some(snapshot) = maybe_snapshot.as_mut() {
        // Carefully preserve name from project manifest if present.
        if let Some(actual_name) = metadata.sync_point_names.get(&file.path) {
            snapshot.name = Cow::Owned(actual_name.clone());
        }
    }

    Ok(maybe_snapshot)
}

fn snapshot_lua_file<'source>(
    file: &'source ImfsFile,
) -> SnapshotResult<'source> {
    let file_name = file.path
        .file_name().expect("Could not extract file stem")
        .to_str().expect("Could not convert path to UTF-8");

    let (instance_name, class_name) = if let Some(name) = match_trailing(file_name, ".server.lua") {
        (name, "Script")
    } else if let Some(name) = match_trailing(file_name, ".client.lua") {
        (name, "LocalScript")
    } else {
        (file_name, "ModuleScript")
    };

    let contents = str::from_utf8(&file.contents)
        .map_err(|inner| SnapshotError::Utf8Error {
            inner,
            path: file.path.to_path_buf(),
        })?;

    Ok(Some(RbxSnapshotInstance {
        name: Cow::Borrowed(instance_name),
        class_name: Cow::Borrowed(class_name),
        properties: hashmap! {
            "Source".to_owned() => RbxValue::String {
                value: contents.to_owned(),
            },
        },
        children: Vec::new(),
        metadata: None,
        source_path: Some(file.path.to_path_buf()),
    }))
}

fn match_trailing<'a>(input: &'a str, trailer: &str) -> Option<&'a str> {
    if input.ends_with(trailer) {
        let end = input.len().saturating_sub(trailer.len());
        Some(&input[..end])
    } else {
        None
    }
}

fn snapshot_txt_file<'source>(
    file: &'source ImfsFile,
) -> SnapshotResult<'source> {
    let instance_name = file.path
        .file_stem().expect("Could not extract file stem")
        .to_str().expect("Could not convert path to UTF-8");

    let contents = str::from_utf8(&file.contents)
        .map_err(|inner| SnapshotError::Utf8Error {
            inner,
            path: file.path.to_path_buf(),
        })?;

    Ok(Some(RbxSnapshotInstance {
        name: Cow::Borrowed(instance_name),
        class_name: Cow::Borrowed("StringValue"),
        properties: hashmap! {
            "Value".to_owned() => RbxValue::String {
                value: contents.to_owned(),
            },
        },
        children: Vec::new(),
        metadata: None,
        source_path: Some(file.path.to_path_buf()),
    }))
}

fn snapshot_csv_file<'source>(
    file: &'source ImfsFile,
) -> SnapshotResult<'source> {
    let instance_name = file.path
        .file_stem().expect("Could not extract file stem")
        .to_str().expect("Could not convert path to UTF-8");

    let entries: Vec<LocalizationEntryJson> = csv::Reader::from_reader(file.contents.as_slice())
        .deserialize()
        // TODO: Propagate error upward instead of panicking
        .map(|result| result.expect("Malformed localization table found!"))
        .map(LocalizationEntryCsv::to_json)
        .collect();

    let table_contents = serde_json::to_string(&entries)
        .expect("Could not encode JSON for localization table");

    Ok(Some(RbxSnapshotInstance {
        name: Cow::Borrowed(instance_name),
        class_name: Cow::Borrowed("LocalizationTable"),
        properties: hashmap! {
            "Contents".to_owned() => RbxValue::String {
                value: table_contents,
            },
        },
        children: Vec::new(),
        metadata: None,
        source_path: Some(file.path.to_path_buf()),
    }))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LocalizationEntryCsv {
    key: String,
    context: String,
    example: String,
    source: String,
    #[serde(flatten)]
    values: HashMap<String, String>,
}

impl LocalizationEntryCsv {
    fn to_json(self) -> LocalizationEntryJson {
        LocalizationEntryJson {
            key: self.key,
            context: self.context,
            example: self.example,
            source: self.source,
            values: self.values,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalizationEntryJson {
    key: String,
    context: String,
    example: String,
    source: String,
    values: HashMap<String, String>,
}

fn snapshot_xml_model_file<'source>(
    file: &'source ImfsFile,
) -> SnapshotResult<'source> {
    let instance_name = file.path
        .file_stem().expect("Could not extract file stem")
        .to_str().expect("Could not convert path to UTF-8");

    let mut temp_tree = RbxTree::new(RbxInstanceProperties {
        name: "Temp".to_owned(),
        class_name: "Folder".to_owned(),
        properties: HashMap::new(),
    });

    let root_id = temp_tree.get_root_id();
    rbx_xml::decode(&mut temp_tree, root_id, file.contents.as_slice())
        .map_err(|inner| SnapshotError::XmlModelDecodeError {
            inner,
            path: file.path.clone(),
        })?;

    let root_instance = temp_tree.get_instance(root_id).unwrap();
    let children = root_instance.get_children_ids();

    match children.len() {
        0 => Ok(None),
        1 => {
            let mut snapshot = snapshot_from_tree(&temp_tree, children[0]).unwrap();
            snapshot.name = Cow::Borrowed(instance_name);
            Ok(Some(snapshot))
        },
        _ => panic!("Rojo doesn't have support for model files with multiple roots yet"),
    }
}

fn snapshot_binary_model_file<'source>(
    file: &'source ImfsFile,
) -> SnapshotResult<'source> {
    let instance_name = file.path
        .file_stem().expect("Could not extract file stem")
        .to_str().expect("Could not convert path to UTF-8");

    let mut temp_tree = RbxTree::new(RbxInstanceProperties {
        name: "Temp".to_owned(),
        class_name: "Folder".to_owned(),
        properties: HashMap::new(),
    });

    let root_id = temp_tree.get_root_id();
    rbx_binary::decode(&mut temp_tree, root_id, file.contents.as_slice())
        .map_err(|inner| SnapshotError::BinaryModelDecodeError {
            inner,
            path: file.path.clone(),
        })?;

    let root_instance = temp_tree.get_instance(root_id).unwrap();
    let children = root_instance.get_children_ids();

    match children.len() {
        0 => Ok(None),
        1 => {
            let mut snapshot = snapshot_from_tree(&temp_tree, children[0]).unwrap();
            snapshot.name = Cow::Borrowed(instance_name);
            Ok(Some(snapshot))
        },
        _ => panic!("Rojo doesn't have support for model files with multiple roots yet"),
    }
}