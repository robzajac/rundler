// This file is part of Rundler.
//
// Rundler is free software: you can redistribute it and/or modify it under the
// terms of the GNU Lesser General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later version.
//
// Rundler is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with Rundler.
// If not, see https://www.gnu.org/licenses/.

use futures::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use libp2p::{core::UpgradeInfo, OutboundUpgrade, Stream};
use tokio_io_timeout::TimeoutStream;
use tokio_util::{codec::Framed, compat::FuturesAsyncReadCompatExt};

use super::{codec, NetworkConfig};
use crate::rpc::{
    message::{Request, ResponseResult},
    protocol::{self, Encoding, Protocol, ProtocolError, ProtocolSchema},
};

#[derive(Debug)]
pub(crate) struct OutboundProtocol {
    pub request: Request,
    pub network_config: NetworkConfig,
}

impl UpgradeInfo for OutboundProtocol {
    type Info = Protocol;
    type InfoIter = Vec<Protocol>;

    fn protocol_info(&self) -> Self::InfoIter {
        protocol::request_protocols(&self.request)
    }
}

impl OutboundUpgrade<Stream> for OutboundProtocol {
    type Output = ResponseResult;
    type Error = ProtocolError;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, socket: Stream, info: Self::Info) -> Self::Future {
        let codec = match info.encoding {
            Encoding::SSZSnappy => codec::OutboundCodec::new(
                info.clone(),
                self.request.num_expected_response_chunks(),
                self.network_config.max_chunk_size,
            ),
        };

        let socket = socket.compat();
        let mut timed_socket = TimeoutStream::new(socket);
        // TODO this should be set after the request is sent.
        timed_socket.set_read_timeout(Some(self.network_config.ttfb_timeout));

        let mut socket = Framed::new(Box::pin(timed_socket), codec);

        async move {
            match info.schema {
                // nothing to send for metadata requests, just close
                ProtocolSchema::MetadataV1 => {}
                _ => socket.send(self.request).await?,
            }

            // close the sink portion of the socket
            socket.close().await?;

            let (response, _stream) = match tokio::time::timeout(
                self.network_config.request_timeout,
                socket.into_future(),
            )
            .await
            {
                Ok((Some(Ok(item)), stream)) => (item, stream),
                Ok((Some(Err(e)), _)) => return Err(ProtocolError::from(e)),
                Ok((None, _)) => return Err(ProtocolError::IncompleteStream),
                Err(_) => return Err(ProtocolError::StreamTimeout),
            };

            Ok(response)
        }
        .boxed()
    }
}
