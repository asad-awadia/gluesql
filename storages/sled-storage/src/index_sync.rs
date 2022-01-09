use {
    super::{err_into, fetch_schema, key, Snapshot},
    gluesql_core::{
        ast::Expr,
        data::{
            schema::{Schema, SchemaIndex},
            Row,
        },
        executor::evaluate_stateless,
        prelude::Value,
        result::{Error, Result},
        store::IndexError,
    },
    sled::{
        transaction::{
            ConflictableTransactionError, ConflictableTransactionResult, TransactionalTree,
        },
        IVec,
    },
    std::borrow::Cow,
    utils::Vector,
};

pub struct IndexSync<'a> {
    tree: &'a TransactionalTree,
    txid: u64,
    table_name: &'a str,
    columns: Vec<String>,
    indexes: Cow<'a, Vec<SchemaIndex>>,
}

impl<'a> IndexSync<'a> {
    pub fn from_schema(tree: &'a TransactionalTree, txid: u64, schema: &'a Schema) -> Self {
        let Schema {
            table_name,
            column_defs,
            indexes,
            ..
        } = schema;

        let columns = column_defs
            .iter()
            .map(|column_def| column_def.name.to_owned())
            .collect::<Vec<_>>();

        let indexes = Cow::Borrowed(indexes);

        Self {
            tree,
            txid,
            table_name,
            columns,
            indexes,
        }
    }

    pub fn new(
        tree: &'a TransactionalTree,
        txid: u64,
        table_name: &'a str,
    ) -> sled::transaction::ConflictableTransactionResult<Self, Error> {
        let Schema {
            column_defs,
            indexes,
            ..
        } = fetch_schema(tree, table_name)
            .map(|(_, snapshot)| snapshot)?
            .map(|snapshot| snapshot.extract(txid, None))
            .flatten()
            .ok_or_else(|| IndexError::ConflictTableNotFound(table_name.to_owned()))
            .map_err(err_into)
            .map_err(ConflictableTransactionError::Abort)?;

        let columns = column_defs
            .into_iter()
            .map(|column_def| column_def.name)
            .collect::<Vec<_>>();

        Ok(Self {
            tree,
            txid,
            table_name,
            columns,
            indexes: Cow::Owned(indexes),
        })
    }

    pub fn insert(&self, data_key: &IVec, row: &Row) -> ConflictableTransactionResult<(), Error> {
        for index in self.indexes.iter() {
            self.insert_index(index, data_key, row)?;
        }

        Ok(())
    }

    pub fn insert_index(
        &self,
        index: &SchemaIndex,
        data_key: &IVec,
        row: &Row,
    ) -> ConflictableTransactionResult<(), Error> {
        let SchemaIndex {
            name: index_name,
            expr: index_expr,
            ..
        } = index;

        let index_key =
            &evaluate_index_key(self.table_name, index_name, index_expr, &self.columns, row)?;

        self.insert_index_data(index_key, data_key)?;

        Ok(())
    }

    pub fn update(
        &self,
        data_key: &IVec,
        old_row: &Row,
        new_row: &Row,
    ) -> ConflictableTransactionResult<(), Error> {
        for index in self.indexes.iter() {
            let SchemaIndex {
                name: index_name,
                expr: index_expr,
                ..
            } = index;

            let old_index_key = &evaluate_index_key(
                self.table_name,
                index_name,
                index_expr,
                &self.columns,
                old_row,
            )?;

            let new_index_key = &evaluate_index_key(
                self.table_name,
                index_name,
                index_expr,
                &self.columns,
                new_row,
            )?;

            self.delete_index_data(old_index_key, data_key)?;
            self.insert_index_data(new_index_key, data_key)?;
        }

        Ok(())
    }

    pub fn delete(&self, data_key: &IVec, row: &Row) -> ConflictableTransactionResult<(), Error> {
        for index in self.indexes.iter() {
            self.delete_index(index, data_key, row)?;
        }

        Ok(())
    }

    pub fn delete_index(
        &self,
        index: &SchemaIndex,
        data_key: &IVec,
        row: &Row,
    ) -> ConflictableTransactionResult<(), Error> {
        let SchemaIndex {
            name: index_name,
            expr: index_expr,
            ..
        } = index;

        let index_key =
            &evaluate_index_key(self.table_name, index_name, index_expr, &self.columns, row)?;

        self.delete_index_data(index_key, data_key)?;

        Ok(())
    }

    fn insert_index_data(
        &self,
        index_key: &[u8],
        data_key: &IVec,
    ) -> ConflictableTransactionResult<(), Error> {
        let data_keys: Vec<Snapshot<Vec<u8>>> = self
            .tree
            .get(index_key)?
            .map(|v| bincode::deserialize(&v))
            .transpose()
            .map_err(err_into)
            .map_err(ConflictableTransactionError::Abort)?
            .unwrap_or_default();

        let key_snapshot = Snapshot::<Vec<u8>>::new(self.txid, data_key.to_vec());
        let data_keys = Vector::from(data_keys).push(key_snapshot);
        let data_keys = bincode::serialize(&Vec::from(data_keys))
            .map_err(err_into)
            .map_err(ConflictableTransactionError::Abort)?;

        let temp_key = key::temp_index(self.txid, index_key);

        self.tree.insert(index_key, data_keys)?;
        self.tree.insert(temp_key, index_key)?;

        Ok(())
    }

    fn delete_index_data(
        &self,
        index_key: &[u8],
        data_key: &IVec,
    ) -> ConflictableTransactionResult<(), Error> {
        let data_keys: Vec<Snapshot<Vec<u8>>> = self
            .tree
            .get(index_key)?
            .map(|v| bincode::deserialize(&v))
            .ok_or_else(|| IndexError::ConflictOnIndexDataDeleteSync.into())
            .map_err(ConflictableTransactionError::Abort)?
            .map_err(err_into)
            .map_err(ConflictableTransactionError::Abort)?;

        let data_keys = data_keys
            .into_iter()
            .map(|snapshot| {
                let key = snapshot.get(self.txid, None);

                if Some(data_key) == key.map(IVec::from).as_ref() {
                    snapshot.delete(self.txid).0
                } else {
                    snapshot
                }
            })
            .collect::<Vec<_>>();

        let data_keys = bincode::serialize(&data_keys)
            .map_err(err_into)
            .map_err(ConflictableTransactionError::Abort)?;

        let temp_key = key::temp_index(self.txid, index_key);

        self.tree.insert(index_key, data_keys)?;
        self.tree.insert(temp_key, index_key)?;

        Ok(())
    }
}

fn evaluate_index_key(
    table_name: &str,
    index_name: &str,
    index_expr: &Expr,
    columns: &[String],
    row: &Row,
) -> ConflictableTransactionResult<Vec<u8>, Error> {
    let evaluated = evaluate_stateless(Some((columns, row)), index_expr)
        .map_err(ConflictableTransactionError::Abort)?;
    let value: Value = evaluated
        .try_into()
        .map_err(ConflictableTransactionError::Abort)?;

    build_index_key(table_name, index_name, value).map_err(ConflictableTransactionError::Abort)
}

pub fn build_index_key_prefix(table_name: &str, index_name: &str) -> Vec<u8> {
    format!("index/{}/{}/", table_name, index_name).into_bytes()
}

pub fn build_index_key(table_name: &str, index_name: &str, value: Value) -> Result<Vec<u8>> {
    Ok(build_index_key_prefix(table_name, index_name)
        .into_iter()
        .chain(value.to_cmp_be_bytes()?)
        .collect::<Vec<_>>())
}
