use alloc::boxed::Box;

use crate::{
    emitter::Emitter,
    message::{Configuration, Flux},
    Error, Identifier,
};

/// Represents a light source containing one or more emitters.
pub trait Source: Send + Sync {
    fn identifier(&self) -> Identifier;
    fn display(
        &mut self,
        config: Configuration,
        target_flux: Flux,
    ) -> Result<(Configuration, Flux), Error>;
    fn emitters(&self) -> &[Box<dyn Emitter>];
}

#[cfg(feature = "remote")]
pub mod remote {
    use crate::{
        message::{
            Command, CommandMessage, Configuration, EmitterCommand, EmitterInfo, Event, Flux,
            SourceCommand, SourceEvent, SourceInfo,
        },
        runtime::remote::RemoteRuntime,
        Identifier,
    };

    /// A source accessed via remote runtime communication.
    ///
    /// RemoteSource wraps a cloned RemoteRuntime and provides access to
    /// source operations through the command/event protocol.
    #[derive(Clone)]
    pub struct RemoteSource {
        info: SourceInfo,
        remote: RemoteRuntime,
    }

    impl RemoteSource {
        /// Create a new RemoteSource with the given runtime and source info.
        pub fn new(info: SourceInfo, remote: RemoteRuntime) -> Self {
            Self { info, remote }
        }

        /// Get the source identifier.
        pub fn identifier(&self) -> Identifier {
            self.info.identifier
        }

        /// Fetch the number of emitters in this source.
        pub async fn emitter_count(&self) -> Result<u32, crate::Error> {
            let command = Command::Source(SourceCommand::EmitterCount);
            let command_message = CommandMessage::root(command, Some(self.identifier()));

            let event_message = self.remote.execute_command(command_message).await?;

            match event_message.event {
                Event::Source(SourceEvent::EmitterCount(count)) => Ok(count),
                _ => Err(crate::Error::UnexpectedResponse),
            }
        }

        /// Send a display command to the source.
        pub async fn display(
            &self,
            config: Configuration,
            target_flux: Flux,
        ) -> Result<(Configuration, Flux), crate::Error> {
            let command = Command::Source(SourceCommand::Display(config, target_flux));
            let command_message = CommandMessage::root(command, Some(self.identifier()));

            let event_message = self.remote.execute_command(command_message).await?;

            match event_message.event {
                Event::Source(SourceEvent::Display(config, flux)) => Ok((config, flux)),
                _ => Err(crate::Error::UnexpectedResponse),
            }
        }

        /// Query emitter info by index.
        pub async fn emitter_info(&self, index: u32) -> Result<EmitterInfo, crate::Error> {
            let command = Command::Source(SourceCommand::EmitterInfo(index));
            let command_message = CommandMessage::root(command, Some(self.identifier()));

            let event_message = self.remote.execute_command(command_message).await?;

            match event_message.event {
                Event::Source(SourceEvent::EmitterInfo(info)) => Ok(info),
                _ => Err(crate::Error::UnexpectedResponse),
            }
        }

        /// Enumerate all emitter identifiers in this source.
        pub async fn emitters(&self) -> Result<Vec<Identifier>, crate::Error> {
            let count = self.emitter_count().await?;
            let mut ids = Vec::new();
            for i in 0..count {
                let info = self.emitter_info(i).await?;
                ids.push(info.identifier);
            }
            Ok(ids)
        }

        /// Set the flux on a specific emitter (fire-and-forget).
        pub async fn set_emitter_flux(
            &self,
            emitter_id: Identifier,
            flux: Flux,
        ) -> Result<(), crate::Error> {
            let command = Command::Emitter(EmitterCommand::FluxSet(flux));
            let command_message = CommandMessage::root(command, Some(emitter_id));
            self.remote.send_command(command_message).await
        }
    }
}
