use tokio_postgres::{Client, NoTls};
use tracing::{error, info};

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
            r#"SELECT "accountID", "accountName", "password", "role", "banned"
                 FROM "account" WHERE "accountName" = $1"#,
            &[&username],
        )
        .await?;
    let Some(row) = rows.first() else {
        return if crate::config::auto_create_accounts() && !username.trim().is_empty() && !password.is_empty() {
            create_account(db, username, password).await.map(Some)
        } else {
            Ok(None)
        };
    };
    if row.get::<_, i16>(4) != 0 {
        info!("{username:?} is banned");
        return Ok(None);
    }

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

/// A player account made at its first login (`autoCreateAccounts`).
async fn create_account(db: &Client, username: &str, password: &str) -> Result<Account, tokio_postgres::Error> {
    let row = db
        .query_one(
            r#"INSERT INTO "account" ("accountName", "password", "role", "createdAt") VALUES ($1, $2, 0, $3) RETURNING "accountID""#,
            &[&username, &password, &crate::bulkdata::filetime_now()],
        )
        .await?;
    info!("account {username:?} made at its first login");
    Ok(Account { account_id: row.get::<_, i32>(0) as i64, name: username.to_string(), role: 0 })
}

/// Makes the config file's accounts, or brings them into line with it:
/// password, GM roles and ban. Returns their accountIDs, in the file's order.
pub async fn sync_accounts(db: &Client, accounts: &[crate::config::Account]) -> Result<Vec<i64>, tokio_postgres::Error> {
    let now = crate::bulkdata::filetime_now();
    // Accounts once made with IDs of their own leave the sequence behind.
    let sql = r#"SELECT setval('"account_accountID_seq"', GREATEST((SELECT max("accountID") FROM "account"), 1))"#;
    db.execute(sql, &[]).await?;
    let mut ids = Vec::with_capacity(accounts.len());
    for a in accounts {
        let banned = i16::from(a.banned);
        let row = db
            .query_one(
                r#"INSERT INTO "account" ("accountName", "password", "role", "banned", "createdAt") VALUES ($1, $2, $3, $4, $5)
                   ON CONFLICT ("accountName") DO UPDATE SET "password" = $2, "passwordAlgo" = 'plain', "role" = $3, "banned" = $4
                   RETURNING "accountID""#,
                &[&a.username, &a.password, &a.role(), &banned, &now],
            )
            .await?;
        ids.push(row.get::<_, i32>(0) as i64);
    }
    Ok(ids)
}
