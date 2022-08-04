//! SeaORM Entity. Generated by sea-orm-codegen 0.9.1

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "user_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub uuid: Vec<u8>,
    pub user_uuid: Vec<u8>,
    #[sea_orm(unique)]
    pub api_key: String,
    pub description: String,
    pub private_txs: i8,
    pub active: i8,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserUuid",
        to = "super::user::Column::Uuid",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    User,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
