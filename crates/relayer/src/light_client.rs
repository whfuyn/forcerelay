pub mod axon;
pub mod eth;
pub mod tendermint;

use core::ops::Deref;

use ibc_proto::google::protobuf::Any;
use ibc_proto::ibc::lightclients::tendermint::v1::Header as RawTmHeader;
use ibc_proto::protobuf::Protobuf as ErasedProtobuf;
use ibc_relayer_types::clients::ics07_axon::header::Header as AxonHeader;
use ibc_relayer_types::clients::ics07_ckb::header::Header as CkbHeader;
use ibc_relayer_types::clients::ics07_eth::header::Header as EthHeader;
use ibc_relayer_types::clients::ics07_tendermint::header::{
    decode_header as tm_decode_header, Header as TendermintHeader, TENDERMINT_HEADER_TYPE_URL,
};
use ibc_relayer_types::core::ics02_client::client_type::ClientType;
use ibc_relayer_types::core::ics02_client::error::Error;
use ibc_relayer_types::core::ics02_client::events::UpdateClient;
use ibc_relayer_types::core::ics02_client::header::Header;
use ibc_relayer_types::timestamp::Timestamp;
use ibc_relayer_types::Height;
use serde::{Deserialize, Serialize};

use crate::chain::endpoint::ChainEndpoint;
use crate::client_state::AnyClientState;
use crate::error;
use crate::misbehaviour::MisbehaviourEvidence;

/// Defines a light block from the point of view of the relayer.
pub trait LightBlock<C: ChainEndpoint>: Send + Sync {
    fn signed_header(&self) -> &C::Header;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified<H> {
    /// Verified target
    pub target: H,
    /// Supporting headers needed to verify `target`
    pub supporting: Vec<H>,
}

/// Defines a client from the point of view of the relayer.
pub trait LightClient<C: ChainEndpoint>: Send + Sync {
    /// Fetch and verify a header, and return its minimal supporting set.
    fn header_and_minimal_set(
        &mut self,
        trusted: Height,
        target: Height,
        client_state: &AnyClientState,
    ) -> Result<Verified<C::Header>, error::Error>;

    /// Fetch a header from the chain at the given height and verify it.
    fn verify(
        &mut self,
        trusted: Height,
        target: Height,
        client_state: &AnyClientState,
    ) -> Result<Verified<C::LightBlock>, error::Error>;

    /// Given a client update event that includes the header used in a client update,
    /// look for misbehaviour by fetching a header at same or latest height.
    fn check_misbehaviour(
        &mut self,
        update: &UpdateClient,
        client_state: &AnyClientState,
    ) -> Result<Option<MisbehaviourEvidence>, error::Error>;

    /// Fetch a header from the chain at the given height, without verifying it
    fn fetch(&mut self, height: Height) -> Result<C::LightBlock, error::Error>;
}

/// Decodes an encoded header into a known `Header` type,
pub fn decode_header(header_bytes: &[u8]) -> Result<Box<dyn Header>, Error> {
    // For now, we only have tendermint; however when there is more than one, we
    // can try decoding into all the known types, and return an error only if
    // none work
    let header: TendermintHeader =
        ErasedProtobuf::<Any>::decode(header_bytes).map_err(Error::invalid_raw_header)?;

    Ok(Box::new(header))
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[allow(clippy::large_enum_variant)]
pub enum AnyHeader {
    Tendermint(TendermintHeader),
    Eth(EthHeader),
    Ckb(CkbHeader),
    Axon(AxonHeader),
}

impl Header for AnyHeader {
    fn client_type(&self) -> ClientType {
        match self {
            Self::Tendermint(header) => header.client_type(),
            Self::Eth(header) => header.client_type(),
            Self::Ckb(header) => header.client_type(),
            Self::Axon(header) => header.client_type(),
        }
    }

    fn height(&self) -> Height {
        match self {
            Self::Tendermint(header) => header.height(),
            Self::Eth(header) => header.height(),
            Self::Ckb(header) => header.height(),
            Self::Axon(header) => header.height(),
        }
    }

    fn timestamp(&self) -> Timestamp {
        match self {
            Self::Tendermint(header) => header.timestamp(),
            Self::Eth(header) => header.timestamp(),
            Self::Ckb(header) => header.timestamp(),
            Self::Axon(header) => header.timestamp(),
        }
    }
}

impl ErasedProtobuf<Any> for AnyHeader {}

impl TryFrom<Any> for AnyHeader {
    type Error = Error;

    fn try_from(raw: Any) -> Result<Self, Error> {
        match raw.type_url.as_str() {
            TENDERMINT_HEADER_TYPE_URL => {
                let val = tm_decode_header(raw.value.deref())?;

                Ok(AnyHeader::Tendermint(val))
            }

            _ => Err(Error::unknown_header_type(raw.type_url)),
        }
    }
}

impl From<AnyHeader> for Any {
    fn from(value: AnyHeader) -> Self {
        match value {
            AnyHeader::Tendermint(header) => Any {
                type_url: TENDERMINT_HEADER_TYPE_URL.to_string(),
                value: ErasedProtobuf::<RawTmHeader>::encode_vec(&header)
                    .expect("encoding to `Any` from `AnyHeader::Tendermint`"),
            },
            AnyHeader::Eth(header) => header.into(),
            AnyHeader::Ckb(header) => header.into(),
            AnyHeader::Axon(header) => header.into(),
        }
    }
}

impl From<TendermintHeader> for AnyHeader {
    fn from(header: TendermintHeader) -> Self {
        Self::Tendermint(header)
    }
}

impl From<EthHeader> for AnyHeader {
    fn from(header: EthHeader) -> Self {
        Self::Eth(header)
    }
}

impl From<CkbHeader> for AnyHeader {
    fn from(header: CkbHeader) -> Self {
        Self::Ckb(header)
    }
}

impl From<AxonHeader> for AnyHeader {
    fn from(header: AxonHeader) -> Self {
        Self::Axon(header)
    }
}
