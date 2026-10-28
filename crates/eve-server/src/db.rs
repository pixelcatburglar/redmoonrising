use tokio_postgres::{Client, NoTls};
use tracing::error;

#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: i64,
    pub name: String,
    pub role: i64,
}

pub async fn connect(url: &str) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("database connection lost: {err}", err = crate::logging::chain(&e));
        }
    });
    Ok(client)
}

pub async fn authenticate(
    db: &Client,
    username: &str,
    password: &str,
) -> Result<Option<Account>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT "accountID", "accountName", "password", "role"
                 FROM "account" WHERE "accountName" = $1"#,
            &[&username],
        )
        .await?;
    let Some(row) = rows.first() else { return Ok(None) };

    let stored: String = row.get(2);
    if stored != password {
        return Ok(None);
    }
    let account_id: i32 = row.get(0);
    Ok(Some(Account {
        account_id: account_id as i64,
        name: row.get(1),
        // bigint: roles above 2^31 exist (ROLE_TRANSLATIONEDITOR = 2^32).
        role: row.get(3),
    }))
}
