mod host_component;

use anyhow::Result;
use redis::{aio::Connection, AsyncCommands, FromRedisValue, Value};
use spin_core::{async_trait, wasmtime::component::Resource};
use spin_world::v1::redis as v1;
use spin_world::v2::redis::{
    self as v2, Connection as RedisConnection, Error, RedisParameter, RedisResult,
};

pub use host_component::OutboundRedisComponent;
use tracing::{instrument, Level};

struct RedisResults(Vec<RedisResult>);

impl FromRedisValue for RedisResults {
    fn from_redis_value(value: &Value) -> redis::RedisResult<Self> {
        fn append(values: &mut Vec<RedisResult>, value: &Value) {
            match value {
                Value::Nil | Value::Okay => (),
                Value::Int(v) => values.push(RedisResult::Int64(*v)),
                Value::Data(bytes) => values.push(RedisResult::Binary(bytes.to_owned())),
                Value::Bulk(bulk) => bulk.iter().for_each(|value| append(values, value)),
                Value::Status(message) => values.push(RedisResult::Status(message.to_owned())),
            }
        }

        let mut values = Vec::new();
        append(&mut values, value);
        Ok(RedisResults(values))
    }
}

pub struct OutboundRedis {
    allowed_hosts: spin_outbound_networking::AllowedHostsConfig,
    connections: table::Table<Connection>,
}

impl Default for OutboundRedis {
    fn default() -> Self {
        Self {
            allowed_hosts: Default::default(),
            connections: table::Table::new(1024),
        }
    }
}

impl OutboundRedis {
    fn is_address_allowed(&self, address: &str) -> bool {
        spin_outbound_networking::check_url(address, "redis", &self.allowed_hosts)
    }

    async fn establish_connection(
        &mut self,
        address: String,
    ) -> Result<Result<Resource<RedisConnection>, Error>> {
        Ok(async {
            let conn = redis::Client::open(address.as_str())
                .map_err(|_| Error::InvalidAddress)?
                .get_async_connection()
                .await
                .map_err(other_error)?;
            self.connections
                .push(conn)
                .map(Resource::new_own)
                .map_err(|_| Error::TooManyConnections)
        }
        .await)
    }
}

impl v2::Host for OutboundRedis {}

// TODO: #[instrument(err)] is only reporting the outer error (if the guest traps), we want to mark
// the span as failed if the inner result is an error too.

#[async_trait]
impl v2::HostConnection for OutboundRedis {
    #[instrument(name = "spin_outbound_redis.open_connection", skip(self), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis"))]
    async fn open(&mut self, address: String) -> Result<Result<Resource<RedisConnection>, Error>> {
        if !self.is_address_allowed(&address) {
            return Ok(Err(Error::InvalidAddress));
        }

        self.establish_connection(address).await
    }

    #[instrument(name = "spin_outbound_redis.publish", skip(self, connection, payload), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("PUBLISH {}", channel)))]
    async fn publish(
        &mut self,
        connection: Resource<RedisConnection>,
        channel: String,
        payload: Vec<u8>,
    ) -> Result<Result<(), Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            conn.publish(&channel, &payload)
                .await
                .map_err(other_error)?;
            Ok(())
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.get", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("GET {}", key)))]
    async fn get(
        &mut self,
        connection: Resource<RedisConnection>,
        key: String,
    ) -> Result<Result<Option<Vec<u8>>, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            let value = conn.get(&key).await.map_err(other_error)?;
            Ok(value)
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.set", skip(self, connection, value), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("SET {}", key)))]
    async fn set(
        &mut self,
        connection: Resource<RedisConnection>,
        key: String,
        value: Vec<u8>,
    ) -> Result<Result<(), Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            conn.set(&key, &value).await.map_err(other_error)?;
            Ok(())
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.incr", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("INCRBY {} 1", key)))]
    async fn incr(
        &mut self,
        connection: Resource<RedisConnection>,
        key: String,
    ) -> Result<Result<i64, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            let value = conn.incr(&key, 1).await.map_err(other_error)?;
            Ok(value)
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.del", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("DEL {}", keys.join(" "))))]
    async fn del(
        &mut self,
        connection: Resource<RedisConnection>,
        keys: Vec<String>,
    ) -> Result<Result<u32, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            let value = conn.del(&keys).await.map_err(other_error)?;
            Ok(value)
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.sadd", skip(self, connection, values), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("SADD {} {}", key, values.join(" "))))]
    async fn sadd(
        &mut self,
        connection: Resource<RedisConnection>,
        key: String,
        values: Vec<String>,
    ) -> Result<Result<u32, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            let value = conn.sadd(&key, &values).await.map_err(|e| {
                if e.kind() == redis::ErrorKind::TypeError {
                    Error::TypeError
                } else {
                    Error::Other(e.to_string())
                }
            })?;
            Ok(value)
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.smembers", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("SMEMBERS {}", key)))]
    async fn smembers(
        &mut self,
        connection: Resource<RedisConnection>,
        key: String,
    ) -> Result<Result<Vec<String>, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            let value = conn.smembers(&key).await.map_err(other_error)?;
            Ok(value)
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.srem", skip(self, connection, values), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("SREM {} {}", key, values.join(" "))))]
    async fn srem(
        &mut self,
        connection: Resource<RedisConnection>,
        key: String,
        values: Vec<String>,
    ) -> Result<Result<u32, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await.map_err(other_error)?;
            let value = conn.srem(&key, &values).await.map_err(other_error)?;
            Ok(value)
        }
        .await)
    }

    #[instrument(name = "spin_outbound_redis.execute", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "redis", otel.name = format!("{}", command)))]
    async fn execute(
        &mut self,
        connection: Resource<RedisConnection>,
        command: String,
        arguments: Vec<RedisParameter>,
    ) -> Result<Result<Vec<RedisResult>, Error>> {
        Ok(async {
            let conn = self.get_conn(connection).await?;
            let mut cmd = redis::cmd(&command);
            arguments.iter().for_each(|value| match value {
                RedisParameter::Int64(v) => {
                    cmd.arg(v);
                }
                RedisParameter::Binary(v) => {
                    cmd.arg(v);
                }
            });

            cmd.query_async::<_, RedisResults>(conn)
                .await
                .map(|values| values.0)
                .map_err(other_error)
        }
        .await)
    }

    fn drop(&mut self, connection: Resource<RedisConnection>) -> anyhow::Result<()> {
        self.connections.remove(connection.rep());
        Ok(())
    }
}

fn other_error(e: impl std::fmt::Display) -> Error {
    Error::Other(e.to_string())
}

/// Delegate a function call to the v2::HostConnection implementation
macro_rules! delegate {
    ($self:ident.$name:ident($address:expr, $($arg:expr),*)) => {{
        if !$self.is_address_allowed(&$address) {
            return Ok(Err(v1::Error::Error));
        }
        let connection = match $self.establish_connection($address).await? {
            Ok(c) => c,
            Err(_) => return Ok(Err(v1::Error::Error)),
        };
        Ok(<Self as v2::HostConnection>::$name($self, connection, $($arg),*)
            .await?
            .map_err(|_| v1::Error::Error))
    }};
}

#[async_trait]
impl v1::Host for OutboundRedis {
    async fn publish(
        &mut self,
        address: String,
        channel: String,
        payload: Vec<u8>,
    ) -> Result<Result<(), v1::Error>> {
        delegate!(self.publish(address, channel, payload))
    }

    async fn get(&mut self, address: String, key: String) -> Result<Result<Vec<u8>, v1::Error>> {
        delegate!(self.get(address, key)).map(|v| v.map(|v| v.unwrap_or_default()))
    }

    async fn set(
        &mut self,
        address: String,
        key: String,
        value: Vec<u8>,
    ) -> Result<Result<(), v1::Error>> {
        delegate!(self.set(address, key, value))
    }

    async fn incr(&mut self, address: String, key: String) -> Result<Result<i64, v1::Error>> {
        delegate!(self.incr(address, key))
    }

    async fn del(&mut self, address: String, keys: Vec<String>) -> Result<Result<i64, v1::Error>> {
        delegate!(self.del(address, keys)).map(|v| v.map(|v| v as i64))
    }

    async fn sadd(
        &mut self,
        address: String,
        key: String,
        values: Vec<String>,
    ) -> Result<Result<i64, v1::Error>> {
        delegate!(self.sadd(address, key, values)).map(|v| v.map(|v| v as i64))
    }

    async fn smembers(
        &mut self,
        address: String,
        key: String,
    ) -> Result<Result<Vec<String>, v1::Error>> {
        delegate!(self.smembers(address, key))
    }

    async fn srem(
        &mut self,
        address: String,
        key: String,
        values: Vec<String>,
    ) -> Result<Result<i64, v1::Error>> {
        delegate!(self.srem(address, key, values)).map(|v| v.map(|v| v as i64))
    }

    async fn execute(
        &mut self,
        address: String,
        command: String,
        arguments: Vec<v1::RedisParameter>,
    ) -> Result<Result<Vec<v1::RedisResult>, v1::Error>> {
        delegate!(self.execute(
            address,
            command,
            arguments.into_iter().map(Into::into).collect()
        ))
        .map(|r| r.map(|v| v.into_iter().map(Into::into).collect()))
    }
}

impl OutboundRedis {
    async fn get_conn(
        &mut self,
        connection: Resource<RedisConnection>,
    ) -> Result<&mut Connection, Error> {
        self.connections
            .get_mut(connection.rep())
            .ok_or(Error::Other(
                "could not find connection for resource".into(),
            ))
    }
}
