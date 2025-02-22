#![deny(clippy::unwrap_used)]

use core::ops::Deref;
use fred::prelude::*;
use jsonwebkey::JsonWebKey;
use jsonwebtoken::{decode, encode, Header, TokenData, Validation};
use lambda_http::http::{
    header::{CONTENT_TYPE, LOCATION, WWW_AUTHENTICATE},
    HeaderValue,
};
use sea_orm::Database;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, env, fmt::Display, ops::DerefMut, str::FromStr};
use vercel_runtime::{Body, Request, Response, StatusCode};

use chrono::{DateTime, Months, Utc};
use entity::prelude::*;
use entity::{auth_grant, auth_token};
use oxide_auth::{
    endpoint::ResponseStatus,
    frontends::{self, simple::endpoint::Vacant},
    primitives::{
        grant::Grant,
        issuer::{IssuedToken, TokenType},
    },
};
use oxide_auth::{
    endpoint::{NormalizedParameter, Scope, WebRequest, WebResponse},
    frontends::dev::Url,
    primitives::registrar::{Client, ClientMap, RegisteredUrl},
};
use oxide_auth_async::primitives::{Authorizer, Issuer};
use oxide_auth_async::{
    endpoint::resource::ResourceFlow, endpoint::Endpoint, endpoint::OwnerSolicitor,
};
use rand::distributions::{Alphanumeric, DistString};
use sea_orm::{prelude::*, ActiveValue};
use sea_orm::{Condition, IntoActiveModel};

use thiserror::Error;

pub mod tfa;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid body type")]
    InvalidBodyType,
}

#[derive(Debug, Default)]
pub struct ResponseCompat(pub Response<vercel_runtime::Body>);

impl Deref for ResponseCompat {
    type Target = Response<vercel_runtime::Body>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ResponseCompat {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<ResponseCompat> for Response<vercel_runtime::Body> {
    fn from(value: ResponseCompat) -> Self {
        value.0
    }
}

impl WebResponse for ResponseCompat {
    type Error = vercel_runtime::Error;

    fn ok(&mut self) -> Result<(), Self::Error> {
        *self.status_mut() = StatusCode::OK;
        Ok(())
    }

    fn body_text(&mut self, text: &str) -> Result<(), Self::Error> {
        self.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_str("text/plain").expect("header to be valid"),
        );
        *self.body_mut() = Body::Text(text.to_owned());

        Ok(())
    }

    fn body_json(&mut self, data: &str) -> Result<(), Self::Error> {
        self.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_str("application/json").expect("header to be valid"),
        );
        *self.body_mut() = Body::Text(data.to_owned());

        Ok(())
    }

    fn redirect(&mut self, url: Url) -> Result<(), Self::Error> {
        self.headers_mut().insert(
            LOCATION,
            HeaderValue::from_str(url.as_ref()).expect("header to be valid"),
        );
        *self.status_mut() = StatusCode::SEE_OTHER;

        Ok(())
    }

    fn client_error(&mut self) -> Result<(), Self::Error> {
        *self.status_mut() = StatusCode::BAD_REQUEST;

        Ok(())
    }

    fn unauthorized(&mut self, header_value: &str) -> Result<(), Self::Error> {
        self.headers_mut().insert(
            WWW_AUTHENTICATE,
            HeaderValue::from_str(header_value).expect("header to be valid"),
        );
        *self.status_mut() = StatusCode::UNAUTHORIZED;

        Ok(())
    }
}

#[derive(Debug)]
pub struct RequestCompat(pub Request);

impl Deref for RequestCompat {
    type Target = Request;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RequestCompat {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<RequestCompat> for Request {
    fn from(value: RequestCompat) -> Self {
        value.0
    }
}

impl WebRequest for RequestCompat {
    type Error = vercel_runtime::Error;
    type Response = ResponseCompat;
    fn authheader(&mut self) -> Result<Option<std::borrow::Cow<str>>, Self::Error> {
        Ok(self.headers().iter().find_map(|(k, v)| {
            if k == "Authorization" {
                Some(Cow::Borrowed(v.to_str().expect("head to be valid string")))
            } else {
                None
            }
        }))
    }

    fn urlbody(
        &mut self,
    ) -> Result<std::borrow::Cow<dyn oxide_auth::endpoint::QueryParameter + 'static>, Self::Error>
    {
        let body: &Body = self.body();

        let encoded = match body {
            Body::Empty => return Err(Box::new(Error::InvalidBodyType)),
            Body::Binary(b) => {
                let encoded = form_urlencoded::parse(b);

                encoded
            }
            Body::Text(t) => {
                let encoded = form_urlencoded::parse(t.as_bytes());

                encoded
            }
        };

        let mut body = NormalizedParameter::new();

        for (k, v) in encoded {
            body.insert_or_poison(Cow::Owned(k.to_string()), Cow::Owned(v.to_string()));
        }

        Ok(Cow::Owned(body))
    }

    fn query(
        &mut self,
    ) -> Result<std::borrow::Cow<dyn oxide_auth::endpoint::QueryParameter + 'static>, Self::Error>
    {
        let url = url::Url::parse(&self.uri().to_string())?;

        let mut params = NormalizedParameter::new();

        for (k, v) in url.query_pairs() {
            params.insert_or_poison(Cow::Owned(k.to_string()), Cow::Owned(v.to_string()));
        }

        Ok(Cow::Owned(params))
    }
}

pub struct ClientData<'a> {
    pub client_id: &'a str,
    pub url: &'a str,
    pub scope: &'a str,
}

pub const VALID_CLIENTS: [ClientData<'static>; 7] = [
    ClientData {
        client_id: "dashboard",
        url: "https://dash.purduehackers.com/api/callback",
        scope: "user:read",
    },
    ClientData {
        client_id: "passports",
        url: "https://passports.purduehackers.com/callback",
        scope: "user:read user",
    },
    ClientData {
        client_id: "authority",
        url: "authority://callback",
        scope: "admin:read admin",
    },
    ClientData {
        client_id: "auth-test",
        url: "https://id-auth.purduehackers.com/api/auth/callback/purduehackers-id",
        scope: "user:read",
    },
    ClientData {
        client_id: "vulcan-auth",
        url: "https://auth.purduehackers.com/source/oauth/callback/purduehackers-id/",
        scope: "user:read",
    },
    ClientData {
        client_id: "shad-moe",
        url: "https://auth.shad.moe/source/oauth/callback/purduehackers-id/",
        scope: "user:read",
    },
    ClientData {
        client_id: "shquid",
        url: "https://www.imsqu.id/auth/callback/purduehackers-id",
        scope: "user:read",
    },
];

pub fn client_registry() -> ClientMap {
    let mut clients = ClientMap::new();

    for ClientData {
        client_id,
        url,
        scope,
    } in VALID_CLIENTS
    {
        clients.register_client(Client::public(
            client_id,
            RegisteredUrl::Semantic(Url::from_str(url).expect("url to be valid")),
            scope.parse().expect("scope to be valid"),
        ));
    }

    clients
}

#[derive(Serialize)]
pub struct APIError<'a> {
    pub message: &'a str,
    pub code: &'a str,
}

pub async fn kv() -> Result<RedisClient, vercel_runtime::Error> {
    let config = RedisConfig::from_url(
        &env::var("KV_URL")
            .expect("KV_URL env var to be present")
            .replace("redis://", "rediss://"),
    )?;
    let c = Builder::from_config(config).build()?;
    c.init().await?;
    Ok(c)
}
pub async fn db() -> Result<DatabaseConnection, vercel_runtime::Error> {
    let db = Database::connect(
        env::var("POSTGRES_URL_NON_POOLING").expect("Database URL var to be present"),
    )
    .await?;
    use migration::{Migrator, MigratorTrait};
    Migrator::up(&db, None).await?;

    Ok(db)
}

/// Vercel makes me do this
pub fn map_error_to_readable<E: Display>(r: Result<Response<Body>, E>) -> Response<Body> {
    match r {
        Ok(r) => r,
        Err(e) => {
            let error = format!("Server Error: {e}");
            println!("{}", &error);
            let mut resp = Response::new(Body::Text(error));
            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            resp
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct PassportRecord {
    pub id: i32,
    pub secret: String,
}

#[macro_export]
macro_rules! wrap_error {
    ($fn:ident) => {
        move |r| {
            Box::pin(async move {
                let res = $fn(r).await;
                Ok($crate::map_error_to_readable(res))
            })
        }
    };
}

pub struct JwtIssuer;

#[async_trait::async_trait]
impl Issuer for JwtIssuer {
    async fn issue(
        &mut self,
        grant: oxide_auth::primitives::grant::Grant,
    ) -> Result<oxide_auth::primitives::prelude::IssuedToken, ()> {
        let until = Utc::now() + Months::new(1);
        let claims = Claims {
            sub: grant.owner_id,
            exp: until.timestamp(),
            iat: Utc::now().timestamp(),
            iss: "id".to_string(),
            aud: grant.client_id,
            scope: grant.scope,
        };

        let jwk = get_jwk();
        let token = encode(
            &Header::new(jwk.algorithm.unwrap().into()),
            &claims,
            &jwk.key.to_encoding_key(),
        )
        .expect("JWT encode success");

        Ok(IssuedToken {
            token,
            refresh: None,
            token_type: TokenType::Bearer,
            until,
        })
    }

    async fn refresh(
        &mut self,
        _: &str,
        _: oxide_auth::primitives::grant::Grant,
    ) -> Result<oxide_auth::primitives::issuer::RefreshedToken, ()> {
        // No refresh tokens
        Err(())
    }

    async fn recover_token(
        &mut self,
        t: &str,
    ) -> Result<Option<oxide_auth::primitives::grant::Grant>, ()> {
        let Ok(TokenData { claims, .. }) = decode::<Claims>(
            t,
            &get_jwk().key.to_decoding_key(),
            &get_validator(IdIsuser::Id),
        ) else {
            return Err(());
        };

        let Some(redirect_uri) = VALID_CLIENTS
            .iter()
            .find(|c| c.client_id == claims.aud)
            .map(|c| c.url)
        else {
            return Err(());
        };

        Ok(Some(Grant {
            owner_id: claims.sub,
            client_id: claims.aud,
            scope: claims.scope,
            until: DateTime::from_timestamp(claims.exp, 0).expect("valid timestamp"),
            extensions: Default::default(),
            redirect_uri: Url::from_str(redirect_uri).expect("valid url"),
        }))
    }

    async fn recover_refresh(
        &mut self,
        _: &str,
    ) -> Result<Option<oxide_auth::primitives::grant::Grant>, ()> {
        // No refresh tokens
        Err(())
    }
}

pub struct DbIssuer;

#[async_trait::async_trait]
impl Issuer for DbIssuer {
    async fn issue(
        &mut self,
        grant: oxide_auth::primitives::grant::Grant,
    ) -> Result<oxide_auth::primitives::prelude::IssuedToken, ()> {
        let db = db().await.expect("db connection to exist");

        let grant: auth_grant::Model = AuthGrant::find()
            .filter(
                Condition::all()
                    .add(
                        auth_grant::Column::OwnerId.eq(grant
                            .owner_id
                            .parse::<i32>()
                            .expect("failed to parse owner_id as int")),
                    )
                    .add(auth_grant::Column::ClientId.eq(grant.client_id.clone())),
            )
            .one(&db)
            .await
            .expect("db op to succeed")
            .expect("grant to be there already");

        let new = auth_token::ActiveModel {
            id: ActiveValue::NotSet,
            grant_id: ActiveValue::Set(grant.id),
            token: ActiveValue::Set(Alphanumeric.sample_string(&mut rand::thread_rng(), 32)),
            until: ActiveValue::Set((Utc::now() + Months::new(1)).into()),
        };

        let new = new.insert(&db).await.expect("insert op to succeed");
        Ok(oxide_auth::primitives::issuer::IssuedToken {
            refresh: None,
            token: new.token,
            token_type: oxide_auth::primitives::issuer::TokenType::Bearer,
            until: new.until.into(),
        })
    }

    async fn refresh(
        &mut self,
        _: &str,
        _: oxide_auth::primitives::grant::Grant,
    ) -> Result<oxide_auth::primitives::issuer::RefreshedToken, ()> {
        // No refresh tokens
        Err(())
    }

    async fn recover_token(
        &mut self,
        t: &str,
    ) -> Result<Option<oxide_auth::primitives::grant::Grant>, ()> {
        let db = db().await.expect("db to be available");

        let token: Option<auth_token::Model> = AuthToken::find()
            .filter(auth_token::Column::Token.eq(t))
            .one(&db)
            .await
            .expect("db op to succeed");

        Ok(match token {
            Some(t) => {
                let grant: auth_grant::Model = t
                    .find_related(AuthGrant)
                    .one(&db)
                    .await
                    .expect("db op to succeed")
                    .expect("token to have grant parent");

                let scope: String =
                    serde_json::from_value(grant.scope).expect("scope to be valid object");
                let redirect_uri: String = serde_json::from_value(grant.redirect_uri)
                    .expect("redirect_uri to be valid object");

                Some(oxide_auth::primitives::grant::Grant {
                    owner_id: grant.owner_id.to_string(),
                    client_id: grant.client_id,
                    scope: scope.parse().expect("scope parse"),
                    extensions: Default::default(),
                    redirect_uri: redirect_uri.parse().expect("redirect uri parse"),
                    until: t.until.into(),
                })
            }
            None => None,
        })
    }

    async fn recover_refresh(
        &mut self,
        _: &str,
    ) -> Result<Option<oxide_auth::primitives::grant::Grant>, ()> {
        // No refresh tokens
        Err(())
    }
}

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String, // Subject (user ID)
    exp: i64,    // Expiration time (timestamp)
    iat: i64,    // Issued at (timestamp)
    iss: String, // Issuer
    aud: String, // Audience
    scope: Scope,
}

/// Not currently in use but can be switched to whenever
pub struct JwtAuthorizer;

pub fn get_jwk() -> JsonWebKey {
    let mut k: JsonWebKey = env::var("JWK")
        .expect("JWK to be present")
        .parse()
        .expect("JWK parse");
    k.set_algorithm(jsonwebkey::Algorithm::ES256)
        .expect("valid algorithm");
    k
}

#[derive(Debug, Clone, Copy)]
enum IdIsuser {
    Id,
    IdGrant,
}

fn get_validator(iss: IdIsuser) -> Validation {
    let mut val = Validation::new(get_jwk().algorithm.expect("algo").into());
    val.set_issuer(&[match iss {
        IdIsuser::Id => "id",
        IdIsuser::IdGrant => "id-grant",
    }]);
    val.set_audience(
        &VALID_CLIENTS
            .iter()
            .map(|c| c.client_id)
            .collect::<Vec<_>>(),
    );

    val
}

#[async_trait::async_trait]
impl Authorizer for JwtAuthorizer {
    async fn authorize(
        &mut self,
        grant: oxide_auth::primitives::grant::Grant,
    ) -> Result<String, ()> {
        let claims = Claims {
            sub: grant.owner_id,
            exp: grant.until.timestamp(),
            iat: Utc::now().timestamp(),
            iss: "id-grant".to_string(),
            aud: grant.client_id,
            scope: grant.scope,
        };

        let jwk = get_jwk();
        let token = encode(
            &Header::new(jwk.algorithm.unwrap().into()),
            &claims,
            &jwk.key.to_encoding_key(),
        )
        .expect("JWT encode success");

        Ok(token)
    }

    async fn extract(&mut self, token: &str) -> Result<Option<Grant>, ()> {
        let Ok(TokenData { claims, .. }) = decode::<Claims>(
            token,
            &get_jwk().key.to_decoding_key(),
            &get_validator(IdIsuser::IdGrant),
        ) else {
            return Err(());
        };

        let Some(redirect_uri) = VALID_CLIENTS
            .iter()
            .find(|c| c.client_id == claims.aud)
            .map(|c| c.url)
        else {
            return Err(());
        };

        Ok(Some(Grant {
            owner_id: claims.sub,
            client_id: claims.aud,
            scope: claims.scope,
            until: DateTime::from_timestamp(claims.exp, 0).expect("valid timestamp"),
            extensions: Default::default(),
            redirect_uri: Url::from_str(redirect_uri).expect("valid url"),
        }))
    }
}

pub struct DbAuthorizer;

#[async_trait::async_trait]
impl Authorizer for DbAuthorizer {
    async fn authorize(
        &mut self,
        grant: oxide_auth::primitives::grant::Grant,
    ) -> Result<String, ()> {
        let db = db().await.expect("db to be accessible");

        let model = auth_grant::ActiveModel {
            id: ActiveValue::NotSet,
            owner_id: ActiveValue::Set(
                grant
                    .owner_id
                    .parse()
                    .expect("failed to parse owner_id as int"),
            ),
            client_id: ActiveValue::Set(grant.client_id),
            redirect_uri: ActiveValue::Set(
                serde_json::to_value(grant.redirect_uri).expect("url value error"),
            ),
            until: ActiveValue::Set(grant.until.into()),
            scope: ActiveValue::Set(
                serde_json::to_value(grant.scope).expect("scope to be serializable"),
            ),
            code: ActiveValue::Set(Some(
                Alphanumeric.sample_string(&mut rand::thread_rng(), 32),
            )),
        };

        let grant = model.insert(&db).await.expect("insert to work");
        Ok(grant.code.expect("grant code to be valid initially"))
    }

    async fn extract(
        &mut self,
        token: &str,
    ) -> Result<Option<oxide_auth::primitives::grant::Grant>, ()> {
        let db = db().await.expect("db to be accessible");

        let grant: Option<auth_grant::Model> = AuthGrant::find()
            .filter(auth_grant::Column::Code.eq(token.to_string()))
            .one(&db)
            .await
            .expect("db op to not fail");

        Ok(match grant {
            Some(g) => {
                let mut am = g.clone().into_active_model();
                am.code = ActiveValue::Set(None);
                am.save(&db).await.expect("db save to work");

                let scope: String =
                    serde_json::from_value(g.scope).expect("scope to be deserializable");
                let uri: String = serde_json::from_value(g.redirect_uri)
                    .expect("redirect uri to be deserializable");
                Some(oxide_auth::primitives::grant::Grant {
                    client_id: g.client_id,
                    extensions: Default::default(),
                    owner_id: g.owner_id.to_string(),
                    scope: Scope::from_str(&scope).expect("scope deserialization from string"),
                    redirect_uri: Url::from_str(&uri).expect("url deserialization from string"),
                    until: g.until.into(),
                })
            }
            None => None,
        })
    }
}

pub struct OAuthEndpoint<T: OwnerSolicitor<RequestCompat>> {
    solicitor: T,
    scopes: Vec<Scope>,
    registry: ClientMap,
    issuer: JwtIssuer,
    authorizer: DbAuthorizer,
}

impl<T: OwnerSolicitor<RequestCompat>> OAuthEndpoint<T> {
    pub fn new(solicitor: T, scopes: Vec<Scope>) -> Self {
        Self {
            solicitor,
            scopes,
            registry: client_registry(),
            issuer: JwtIssuer,
            authorizer: DbAuthorizer,
        }
    }
}

#[async_trait::async_trait]
impl<T: OwnerSolicitor<RequestCompat> + Send> Endpoint<RequestCompat> for OAuthEndpoint<T> {
    type Error = vercel_runtime::Error;

    fn web_error(&mut self, err: <RequestCompat as WebRequest>::Error) -> Self::Error {
        format!("OAuth Web Error: {err}").into()
    }

    fn error(&mut self, err: frontends::dev::OAuthError) -> Self::Error {
        format!("OAuth Error: {err}").into()
    }

    fn owner_solicitor(&mut self) -> Option<&mut (dyn OwnerSolicitor<RequestCompat> + Send)> {
        Some(&mut self.solicitor)
    }

    fn scopes(&mut self) -> Option<&mut dyn oxide_auth::endpoint::Scopes<RequestCompat>> {
        Some(&mut self.scopes)
    }

    fn response(
        &mut self,
        _request: &mut RequestCompat,
        mut kind: oxide_auth::endpoint::Template,
    ) -> Result<<RequestCompat as WebRequest>::Response, Self::Error> {
        if let Some(e) = kind.authorization_error() {
            return Err(format!("Auth error: {e:?}").into());
        }
        if let Some(e) = kind.access_token_error() {
            return Err(format!("Access token error: {e:?}").into());
        }

        match kind.status() {
            ResponseStatus::Ok | ResponseStatus::Redirect => {
                Ok(ResponseCompat(Response::new(Body::Empty)))
            }
            ResponseStatus::BadRequest => Err("Bad request".to_string().into()),
            ResponseStatus::Unauthorized => Err("Unauthorized".to_string().into()),
        }
    }

    fn registrar(&self) -> Option<&(dyn oxide_auth_async::primitives::Registrar + Sync)> {
        Some(&self.registry)
    }

    fn issuer_mut(&mut self) -> Option<&mut (dyn oxide_auth_async::primitives::Issuer + Send)> {
        Some(&mut self.issuer)
    }

    fn authorizer_mut(
        &mut self,
    ) -> Option<&mut (dyn oxide_auth_async::primitives::Authorizer + Send)> {
        Some(&mut self.authorizer)
    }
}

pub async fn oauth_user(req: Request, scopes: Vec<Scope>) -> Result<i32, vercel_runtime::Error> {
    let user = ResourceFlow::prepare(OAuthEndpoint::new(Vacant, scopes))
        .map_err(|e| format!("Resource flow prep error: {e:?}"))?
        .execute(RequestCompat(req))
        .await
        .map_err(|e| format!("Resource flow exec error: {e:?}"))?;

    Ok(user.owner_id.parse().expect("db id to be i32"))
}
