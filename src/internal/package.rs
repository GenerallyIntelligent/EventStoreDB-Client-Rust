use bytes::Bytes;
use protobuf::{ Message, MessageStatic, parse_from_bytes };
use uuid::Uuid;

use internal::command::Cmd;
use internal::messages;
use types::Credentials;

pub struct Pkg {
    pub cmd:         Cmd,
    pub creds_opt:   Option<Credentials>,
    pub correlation: Uuid,
    pub payload:     Vec<u8>,
}

static CLIENT_VERSION: i32 = 1;
pub static PKG_MANDATORY_SIZE: usize = 18;

impl Pkg {
    pub fn new(cmd: Cmd, correlation: Uuid) -> Pkg {
        Pkg {
            cmd:         cmd,
            creds_opt:   None,
            correlation: correlation,
            payload:     Vec::new(),
        }
    }

    pub fn from_message<M>(cmd: Cmd, creds_opt: Option<Credentials>, msg: &M)
        -> ::std::io::Result<Pkg>
        where M: Message
    {
        let     size  = msg.compute_size() as usize;
        let mut bytes = Vec::with_capacity(size);

        msg.write_to_vec(&mut bytes)?;

        let pkg = Pkg {
            cmd,
            creds_opt,
            correlation: Uuid::new_v4(),
            payload: bytes,
        };

        Ok(pkg)
    }

    pub fn set_payload(&mut self, payload: Vec<u8>) {
        self.payload = payload;
    }

    pub fn set_credentials(&mut self, creds: Credentials) {
        self.creds_opt = Some(creds);
    }

    pub fn size(&self) -> usize {
        let creds_size = {
            match self.creds_opt {
                Some(ref creds) => creds.login.len() + creds.password.len() + 2, // Including 2 length bytes.
                None            => 0,
            }
        };

        PKG_MANDATORY_SIZE + self.payload.len() + creds_size
    }

    pub fn heartbeat_request() -> Pkg {
        Pkg::new(Cmd::HeartbeatRequest, Uuid::new_v4())
    }

    pub fn authenticate(creds: Credentials) -> Pkg {
        let corr_id = Uuid::new_v4();
        let mut pkg = Pkg::new(Cmd::Authenticate, corr_id);

        pkg.set_credentials(creds);

        pkg
    }

    pub fn identify_client(name_opt: &Option<String>) -> Pkg {
        let     corr_id = Uuid::new_v4();
        let mut pkg     = Pkg::new(Cmd::IdentifyClient, corr_id);
        let mut msg     = messages::IdentifyClient::new();
        let     name    = match *name_opt {
            Some(ref name) => name.clone(),
            None           => format!("ES-{}", Uuid::new_v4()),
        };

        msg.set_connection_name(name);
        msg.set_version(CLIENT_VERSION);
        msg.write_to_vec(&mut pkg.payload).unwrap();

        pkg
    }

    // Copies the Pkg except its payload.
    pub fn copy_headers_only(&self) -> Pkg {
        Pkg {
            cmd:         self.cmd,
            correlation: self.correlation,
            payload:     Vec::new(),
            creds_opt:   None,
        }
    }

    pub fn to_message<M>(&self) -> ::std::io::Result<M>
        where M: MessageStatic
    {
        parse_from_bytes(self.payload.as_slice()).map_err(|e| e.into())
    }
}
