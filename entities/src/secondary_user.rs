//! SeaORM Entity. Generated by sea-orm-codegen 0.9.1

use super::sea_orm_active_enums::Role;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "secondary_user")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub uuid: Vec<u8>,
    pub user_id: Vec<u8>,
    pub address: String,
    pub description: String,
    pub email: String,
    pub role: Role,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
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
