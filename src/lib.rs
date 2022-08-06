//! This is a library designed to ease the creation of a collection of singleton services namespaced to a single
//! base directory path with an arbitrary network of dependencies, starting services as needed,
//! using unix sockets as the communication mechanism.
//!
//! In the case that a service already exists, then this will communicate with the appropriate
//! socket, rather than start a new one.
//!
//! To get started, take a look at the [`declare_service`] macro.

mod cleanable_path;
pub mod mapfut;
pub mod socket_shims;
pub mod timefut;

/// Provide async_trait for convenience.
pub use async_trait::async_trait;
use cleanable_path::CleanablePathBuf;
pub use futures_lite::future;

use socket_shims::{DefaultUnixSocks, UnixSocketImplementation};
use std::{ffi::OsStr, fmt::Debug, future::Future, os::unix::net::UnixListener, path::Path};
use std::{io::Result as IoResult, os::unix::net::UnixStream, process::Child, time::Duration};
use timefut::with_timeout;
use tracing::{error, info, instrument, trace, warn};

/// Trait used to define a single, startable service, with a relative socket path. For a more
/// concise way of implementing services, take a look at the [`declare_service`] and
/// [`declare_service_bundle`] macros that let you implement this trait far more concisely. For the
/// items emitted by service bundles, have a look at [`ReifiedService`], which embeds an executor
/// prefix and base context directory along with an abstract service implementation encoded by this
/// trait.
///
/// A service - in `suss` terminology - is a process that can be communicated with
/// through a [`std::os::unix::net::UnixStream`].
///
/// Services are run with a *base context path*, which acts as a runtime namespace and
/// allows multiple instances of collections of services without accidental interaction
/// between the groups - for instance, if you wanted a service to run once per user, you
/// could set the context directory as somewhere within $HOME or a per-user directory.
///
/// Services can also be provided an optional *executor prefix* - this is something that - in the
/// case of command execution to start a service, should be added to the start of all service
/// commands as the actual executable to run. This is useful for some cases:
/// * It may be useful to run anything with a prefix of /usr/bin/env in certain operating
///   environments like nixos
/// * It can be used to override implementations with a custom version of certain services
/// * It can be used to instrument services with some kind of notification or liveness system
///   outside of the one internally managed by this process
/// * Anything else you can think of, as long as it ends up delegating to something that actually
///   runs a valid service, or maybe fails if you want to conditionally prevent services from
///   functioning.
///
/// Each service has an associated *socket name*, which is a place in the *context base path*
/// that services put their receiver unix sockets. Services are checked for running-status by
/// if their associated socket files exist (and can be connected to) - i.e. they try to create a
/// `UnixStream` and if it fails, try to start the service.
///
/// Socket files, actually running the service, etc. are not handled by this trait. Instead, they
/// are handled by a [`ServerService`], which takes care of things like cleaning up socket files
/// afterward automatically in [`Drop`]
#[async_trait]
pub trait Service: Debug + Sync {
    /// A connection to the service server - must be generatable from a bare
    /// [`std::os::unix::net::UnixStream`]
    type ServiceClientConnection: Send;

    /// Obtain the name of the socket file in the base context path. In your collection of
    /// services, the result should be unique, or you might end up with service collisions when
    /// trying to grab sockets.
    fn socket_name(&self) -> &std::ffi::OsStr;

    /// Convert a bare unix stream into a [`Self::ServiceClientConnection`]
    fn wrap_connection(&self, bare_stream: UnixStream) -> IoResult<Self::ServiceClientConnection>;

    /// This should *synchronously* attempt to start the service, with the given ephemeral liveness
    /// socket path passed through if present to that service (probably by way of a command line
    /// argument).
    ///
    /// The provided executor argument list should be prefixed to any commandline executions if possible -
    /// it provides a convenient means of allowing replaceable and instrumentable services.
    ///
    /// The ephemeral socket path should be connected to and then immediately shut down
    /// by the running service process (this is handled automatically by [`ServerService`] if you
    /// use that to run your service).
    ///
    /// Ephemeral liveness check timeouts are applied by the library later on.
    fn run_service_command_raw(
        &self,
        executor_commandline_prefix: Option<&[&OsStr]>,
        liveness_path: Option<&Path>,
    ) -> IoResult<Child>;

    /// This function is applied to the child process after it has passed the liveness check but
    /// before it has been connected to. In here you can add it to a threadpool or something if you want to
    /// .wait on it. Bear in mind it is an async function so don't block.
    ///
    /// The default version of this function will simply drop the child and leave it a zombie -
    /// this is desired if you want the services to be more persistent, but if you want to tie the
    /// lifetime of the service to the lifetime of the parent process, spawning a task that just
    /// .wait()s on the child or does some async equivalent may be sufficient. Well, it might also
    /// block your own process until the child dies but hey ho!, sort that out yourself :) - you
    /// probably want to use your runtime's equivalent of `spawn` for this.
    ///
    /// Of course this function, like the [`Self::run_service_command_raw`] function, are not used at all
    /// if the service already exists in base context directory.
    async fn after_post_liveness_subprocess(&self, _: Child) -> IoResult<()> {
        Ok(())
    }
}

/// Utility function to obtain a random path in [`std::env::tempdir`], of the form
/// `$tempdir/temp-XXXXXXXXXXXXXXXX.sock` (16 xs), where the x's are replaced by numbers
/// from 0-9a-f (hex)
fn get_random_sockpath() -> std::path::PathBuf {
    use nanorand::rand::{chacha::ChaCha20, Rng};
    let mut path = std::env::temp_dir();
    let mut gen = ChaCha20::new();
    // 1 byte => 2 chars
    // 16 chars => 8 bytes => 64 bits => u64
    path.push(format!("temp-{:016x}.sock", gen.generate::<u64>()));
    path
}

#[async_trait]
pub trait ServiceExt: Service {
    /// Reify this [`Service`] into a [`ReifiedService`] that carries around necessary context for
    /// connecting to it.
    fn reify(self, base_context_directory: &Path) -> ReifiedService<'_, Self>
    where
        Self: Sized,
    {
        ReifiedService::reify_service(self, base_context_directory)
    }

    /// Reify this [`Service`] into a [`ReifiedService`] that carries around necessary context for
    /// connecting to it, including an executor prefix command.
    fn reify_with_executor<'i>(
        self,
        base_context_directory: &'i Path,
        executor_prefix: &'i [&'i OsStr],
    ) -> ReifiedService<'i, Self>
    where
        Self: Sized,
    {
        ReifiedService::reify_service_with_executor(self, base_context_directory, executor_prefix)
    }

    /// Attempt to connect to an already running service. This will not try to start the service on
    /// failure - for that, see [`Self::connect_to_service`]
    ///
    /// See [`Service`] for information on base context directories.
    #[instrument]
    async fn connect_to_running_service(
        &self,
        base_context_directory: &Path,
    ) -> IoResult<<Self as Service>::ServiceClientConnection> {
        use crate::socket_shims::UnixSocketImplementation;
        let server_socket_path = base_context_directory.join(<Self as Service>::socket_name(self));
        info!(
            "Attempting connection to service @ {}",
            server_socket_path.display()
        );
        match DefaultUnixSocks::us_connect(&server_socket_path).await {
            Ok(non_std_unix_stream) => {
                info!("Successfully obtained async unix socket");
                trace!("Attempting conversion to std::os::unix::net::UnixStream");
                let std_unix_stream = DefaultUnixSocks::us_to_std(non_std_unix_stream)?;
                trace!("Wrapping into the final client connection...");
                self.wrap_connection(std_unix_stream)
            }
            Err(e) => {
                error!(
                    "Failed to connect to service @ {}",
                    server_socket_path.display()
                );
                Err(e)
            }
        }
    }

    /// Attempt to connect to the given service in the given runtime context directory.
    ///
    /// See [`Service`] for information on executor commandline prefixes and the base context
    /// directory.
    ///
    /// If the service is not already running, then `liveness_timeout` is the maximum time before a
    /// non-response to the liveness check will result in an error.
    #[instrument]
    async fn connect_to_service(
        &self,
        executor_commandline_prefix: Option<&[&OsStr]>,
        base_context_directory: &Path,
        liveness_timeout: Duration,
    ) -> IoResult<<Self as Service>::ServiceClientConnection> {
        use socket_shims::UnixSocketImplementation;
        match self
            .connect_to_running_service(base_context_directory)
            .await
        {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!("Error connecting to existing service - {} - attempting on-demand service start", e);
                let ephemeral_socket_path = CleanablePathBuf::new(get_random_sockpath());
                info!(
                    "Creating ephemeral liveness socket @ {}",
                    ephemeral_socket_path.as_ref().display()
                );
                let ephem = DefaultUnixSocks::ul_bind(ephemeral_socket_path.as_ref())
                    .await
                    .map_err(|e| {
                        error!(
                            "Couldn't create ephemeral liveness socket @ {} - {}",
                            ephemeral_socket_path.as_ref().display(),
                            e
                        );
                        e
                    })?;

                // We have an ephemeral socket, so begin running the child process, using `unblock`
                let child_proc = self
                    .run_service_command_raw(
                        executor_commandline_prefix,
                        Some(ephemeral_socket_path.as_ref()),
                    )
                    .map_err(|e| {
                        error!("Could not start child service process - {}", e);
                        e
                    })?;

                // Now wait for a liveness ping
                let mut temp_unix_stream = with_timeout(
                    DefaultUnixSocks::ul_try_accept_connection(&ephem),
                    liveness_timeout,
                )
                .await
                .unwrap_or_else(|| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "Timed out waiting for service to become live after {}",
                            humantime::format_duration(liveness_timeout)
                        ),
                    ))
                })
                .map_err(|e| {
                    error!(
                        "Failed to receive liveness ping for service on ephemeral socket {} - {}",
                        ephemeral_socket_path.as_ref().display(),
                        e
                    );
                    e
                })?;

                DefaultUnixSocks::us_shutdown(&mut temp_unix_stream).await?;
                drop(temp_unix_stream);
                drop(ephem);
                drop(ephemeral_socket_path);

                self.after_post_liveness_subprocess(child_proc).await?;
                info!("Successfully received ephemeral liveness ping - trying to connect to service again.");
                self.connect_to_running_service(base_context_directory)
                    .await
            }
        }
    }
}

impl<S: Service> ServiceExt for S {}

/// Represents a running service on a [`UnixListener`]. The unix socket can be preprocessed and
/// wrapped in some other type that may encapsulate listening behaviour beyond bare socket
/// communication.
///
/// When this object is consumed, it will destroy the unix listener and delete the socket file
/// automatically.
pub struct ServerService<ServiceSpec: Service, SocketWrapper = UnixListener> {
    service: ServiceSpec,
    unix_listener_socket: SocketWrapper,
    socket_path: CleanablePathBuf,
}

impl<ServiceSpec: Service, SocketWrapper> Debug for ServerService<ServiceSpec, SocketWrapper> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerService")
            .field("service", &self.service)
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

impl<ServiceSpec: Service, SocketWrapper> ServerService<ServiceSpec, SocketWrapper> {
    /// This attempts to initially open the unix socket with the name appropriate to the service
    /// (see [`Service`]), in the current service namespace directory (the context base path). You
    /// must provide a means of converting a raw unix listener socket into a `SocketWrapper` that
    /// you can then use - this method can be fallible.
    ///
    /// In implementations, `SocketWrapper` is generally some kind of higher-level
    /// abstraction that provides things like RPC connection protocols or some kind of type
    /// encoding/decoding overlay for the raw connection.
    ///
    /// This function is where such wrappers are created - though you can of course use an identity
    /// function and work with raw sockets if you really want to all the way down. This function
    /// currently is synchronous, but as far as the author of this library knows most async
    /// runtimes allow easy translation between std sockets and the async sockets, so you can use
    /// them at-will.
    #[instrument(skip(unix_listener_wrapping))]
    pub fn try_and_open_raw_socket(
        service: ServiceSpec,
        context_base_path: &Path,
        unix_listener_wrapping: impl FnOnce(UnixListener) -> IoResult<SocketWrapper>,
    ) -> IoResult<Self> {
        let socket_path: CleanablePathBuf = context_base_path.join(service.socket_name()).into();
        let raw_listener = UnixListener::bind(&socket_path)?;
        Ok(Self {
            service,
            unix_listener_socket: unix_listener_wrapping(raw_listener)?,
            socket_path,
        })
    }

    /// Runs an arbitrary async service server, consuming the service after performing relevant
    /// liveness protocols.
    ///
    /// Arguments:
    ///  * the service function runs on the initially provided socket wrapper, and returns some
    ///  arbitrary result.
    ///
    ///  * The liveness path is a component of the mechanism by which this library allows one
    ///  service to start another. When provided, it is a path to an ephemeral unix socket that the
    ///  parent service listens on for a single connection. The act of connecting to it indicates
    ///  that the main service socket is open - which is done during the creation of this structure.
    ///  This provides several uses:
    ///     * It means that the moment a connection is received by the parent service or process, it
    ///     can connect to this service, after starting the current service. This avoids things
    ///     like polling.
    ///     * It avoids PID data races in the case that a PID file is being used to indicate
    ///     liveness - a unique socket address prevents a new process starting after premature
    ///     termination, with the same PID, creating a PIDfile.
    ///  Being unable to connect to the liveness path is not an error - the parent process probably
    ///  unexpectedly died, but the processes involved are generally speaking "persistent on-demand".
    ///  
    ///  * `die_with_parent_prefailure` tells the server function to error out if a liveness path is
    ///  provided and yet the unix socket can't be connected to - probably something to do with the
    ///  parent process being dead. This ensures that there is an overlap in the lifetime of the
    ///  parent process and the lifetime of this service.
    ///
    /// Uses [`socket_shims::DefaultUnixSocks`] for creating and managing any temporary sockets
    /// asynchronously.
    #[instrument(skip(the_service_function))]
    pub async fn run_server<T, F: Future<Output = IoResult<T>>>(
        self,
        the_service_function: impl FnOnce(SocketWrapper, ServiceSpec) -> F,
        liveness_path: Option<&Path>,
        die_with_parent_prefailure: bool,
    ) -> IoResult<T> {
        match liveness_path {
            Some(parent_socket_path) => {
                match DefaultUnixSocks::us_connect(parent_socket_path).await {
                    Ok(mut raw_socket) => {
                        info!("Ping'ed liveness socket @ {} with connection, shutting ephemeral connection.", parent_socket_path.display());
                        let _ = DefaultUnixSocks::us_shutdown(&mut raw_socket).await;
                    }
                    Err(ephemeral_error) => {
                        if die_with_parent_prefailure {
                            error!("Could not connect to parent process's ephemeral liveness socket @ {}", parent_socket_path.display());
                            return Err(ephemeral_error);
                        } else {
                            warn!("Couldn't connect to parent process's ephemeral liveness socket @ {} - continuing service anyway, error was: {}", parent_socket_path.display(), ephemeral_error);
                            drop(ephemeral_error);
                        }
                    }
                }
            }
            None => info!("No liveness path, assuming autonomous"),
        };
        let Self {
            service,
            unix_listener_socket,
            socket_path,
        } = self;

        let res = the_service_function(unix_listener_socket, service).await;
        drop(socket_path);
        res
    }
}

#[derive(Debug)]
/// Holds a particular instance of a [`Service`], along with a base context directory and optional
/// executor prefix.
///
/// This lets you interact with services in a manner not requiring you to carry around
/// base_context_directories and executor_prefixes, and they are what [`ServiceBundle`]s produce
/// for you under the hood.
pub struct ReifiedService<'info, S: Service> {
    executor_prefix: Option<&'info [&'info OsStr]>,
    base_context_directory: &'info Path,
    bare_service: S,
}

impl<'info, S: Service + Sync> ReifiedService<'info, S> {
    /// Reify a service into a specific base context directory
    pub fn reify_service(service: S, base_context_directory: &'info Path) -> Self {
        Self {
            executor_prefix: None,
            base_context_directory,
            bare_service: service,
        }
    }

    /// Reify a service along with an executor prefix.
    pub fn reify_service_with_executor(
        service: S,
        base_context_directory: &'info Path,
        executor_prefix: &'info [&'info OsStr],
    ) -> Self {
        Self {
            executor_prefix: Some(executor_prefix),
            base_context_directory,
            bare_service: service,
        }
    }

    /// Connect to this [`Service`], trying to start it if not possible.
    ///
    /// The timeout is for how long to wait until concluding that - in the case we attempted to
    /// start a service because it wasn't running - the service failed to begin.
    ///
    /// If you don't care about starting the service on-demand, take a look at
    /// [`Self::connect_to_running`]
    #[instrument]
    pub async fn connect(
        &self,
        liveness_timeout: Duration,
    ) -> IoResult<S::ServiceClientConnection> {
        self.bare_service
            .connect_to_service(
                self.executor_prefix,
                self.base_context_directory,
                liveness_timeout,
            )
            .await
    }

    /// Connect to this [`Service`], without attempts to start it upon failure.
    ///
    /// If you want to try and start the service on-demand, take a look at [`Self::connect`]
    #[instrument]
    pub async fn connect_to_running(&self) -> IoResult<S::ServiceClientConnection> {
        self.bare_service
            .connect_to_running_service(self.base_context_directory)
            .await
    }
}

/// Trait implemented by "bundles" of services that all work together and call each other.
///
/// Provides a unified interface for applying *base context directories* and *executor commands* to
/// all of a collection of services, to then instantiate a defined service (these are inherent impl
/// methods on generated types).
pub trait ServiceBundle {
    /// Create the service bundle with the given base context directory,
    fn new(base_context_directory: &Path) -> Self;

    /// Create the service bundle with the base context directory, along with an executor prefix
    fn with_executor_prefix(base_context_directory: &Path, executor_prefix: &[&OsStr]) -> Self;
}

#[macro_export]
/// A macro that aids in generating the common case of a service that has a command name and calls
/// out to a command.
///
/// This creates unit-type items that implement the [`Service`] trait, where the services are
/// started by standard [`std::process::Command`] execution - including correct executor prefix
/// implementation.
///
/// Wrapping [`std::os::unix::net::UnixStream`]s in higher-level abstractions can be specified in a
/// number of ways (in future, currently we only implement bare functions). These methods are
/// called *USP*s (**U**nix **S**tream **P**reprocessors), of which there is currently one (though
/// it should be able to implement any other with sufficient effort).
///
/// Using this macro goes something like the following:
///
/// ```rust
/// use suss::declare_service;
///
/// declare_service! {
///     /// My wonderful service
///     pub WonderfulService = {
///         "some-wonderful-command" [
///             "always-present" "arguments" | "arguments" "before" "liveness"
///             "path" "when" "present" {} "arguments" "after" "liveness" "path" "when"
///             "present" | "always-present" "arguments"
///         ] @ "unix-socket-filename.sock"
///         as some_usp_method some_usp_method_specifications
///     }
/// }
/// ```
///
/// Services are just unit types in this case, and can have any visibility you like and
/// documentation or other things like `#[derive]` on them as desired.
///
/// The first part of the definition controls what command to run to execute the service. Inside
/// the square brackets, there are 3 clear sections.
///
/// The first and last sections are arguments that are always present when executing the command.
/// The middle section is arguments only present when that service is being passed a liveness socket
/// path as documented in [`ServerService::run_server`]
///
/// The literal after the @ is the name of the socket within the *base context directory* that
/// this service hosts itself upon. For example, if your base context directory is `/var/run`, and
/// the socket name for a service is `hello-service.sock`, then the service should receive
/// connections on `/var/run/hello-service.sock`.
///
/// Note that there is *no easy way* to pass in the base context directory to the command. This is
/// a concious decision - this library is designed for *services*, not just *subprocesses*, and
/// hence other programs should be able to find a service via some method derived from the
/// environment.
///
/// If nothing else, storing a context directory in an environment variable will do
/// the trick, but the point is that generally the base context directory should be defined by
/// environment, whether that be `XDG`, or a global fixed directory, or an environment variable, or
/// any combination of the above or some other environmental context.
// TODO: Perhaps change liveness socket information to an environment variable to avoid polluting
// the CLI?
///
/// This defines how a service is started and how to locate it. The stuff after the *as* provides
/// information on what to do once you've got a connection.
///
/// ### Methods
///
/// #### Raw
///
/// The `raw` method is essentially an arbitrary function that takes a
/// [`std::os::unix::net::UnixStream`] and produces (wrapped in a [`std::io::Result`]), a
/// higher-level abstraction over the stream that the rest of the world will have access to.
///
/// ```rust
///  ...rest-of-arg... as raw |name_of_raw_std_unix_socket_variable| -> Io<abstracted_and_wrapped_connection_type> {
///     Ok(some_wrapped_type)
///  }
/// ```
macro_rules! declare_service {
    {
        $(#[$service_meta:meta])*
        $vis:vis $service_name:ident = {
            $command:literal [ $($pre_args:literal)* | $($liveness_pre_args:literal)* {} $($liveness_post_args:literal)* | $($post_args:literal)* ] @ $socket_name:literal
                as $unix_stream_preprocess_method:ident $($unix_stream_preprocess_spec:tt)*
        }
    } => {
        $(#[$service_meta])*
        #[derive(Debug)]
        $vis struct $service_name;

        impl $crate::Service for $service_name {
            type ServiceClientConnection = $crate::declare_service!(@socket_connection_type $unix_stream_preprocess_method $($unix_stream_preprocess_spec)*);

            #[inline]
            fn socket_name(&self) -> &::std::ffi::OsStr {
                ::std::ffi::OsStr::new($socket_name)
            }

            #[inline]
            fn wrap_connection(&self, bare_stream: ::std::os::unix::net::UnixStream) -> IoResult<Self::ServiceClientConnection> {
                $crate::declare_service!(@wrap_implementation bare_stream $unix_stream_preprocess_method $($unix_stream_preprocess_spec)*)
            }

            fn run_service_command_raw(
                &self,
                executor_commandline_prefix: ::core::option::Option<&[&::std::ffi::OsStr]>,
                liveness_path: ::core::option::Option<&::std::path::Path>,
            ) -> ::std::io::Result<::std::process::Child> {
                use ::std::{process::Command, iter::{Iterator, IntoIterator, once}, ffi::OsStr};
                // Build an iterator out of all the CLI components and unconditionally take the
                // first. This ends up being generally simpler in the long run than trying to wrangle
                // matches and conditional inclusion of items.
                let mut all_components_iterator = executor_commandline_prefix
                    .map(|l| l.iter().cloned()).into_iter()
                    .flatten()
                    // This is the part that ensures that at least the first element always exists.
                    .chain(once(OsStr::new($command)))
                    // Arguments before the condiitonal liveness
                    .chain([$(OsStr::new($pre_args)),*].into_iter())
                    // transform the liveness optional into an optional iterator, then flatten
                    // because option is itself an iterator
                    .chain(liveness_path.map(|real_liveness| {
                        [$(OsStr::new($liveness_pre_args)),*].into_iter()
                            .chain(once(real_liveness.as_os_str()))
                            .chain([$(OsStr::new($liveness_post_args)),*])
                    }).into_iter().flatten())
                    // last arguments.
                    .chain([$(OsStr::new($post_args)),*].into_iter());

                let program = all_components_iterator.next().expect("There must be at least one thing in the iterator - the program to run, itself.");
                Command::new(program)
                    .args(all_components_iterator)
                    .spawn()
            }
        }
    };
    // macro "method" for extracting the result type from the preprocess method and specification
    {@socket_connection_type raw |$unix_socket:ident| -> Io<$result:ty> $body:block } => { $result };
    // macro "method" for implementing the connection wrapper stuff
    {@wrap_implementation $stream_ident:ident raw |$unix_socket:ident| -> Io<$result:ty> $body:block} => {{
        let inner_closure = |$unix_socket| -> ::std::io::Result<$result> { $body };
        inner_closure($stream_ident)
    }};
}

/// Module for usually-necessary imports.
pub mod prelude {
    pub use super::declare_service;
    pub use super::ServerService;
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;

    use futures_lite::future::block_on;

    use super::*;

    #[test]
    pub fn service_declaration_and_start_fail_test() {
        use std::os::unix::net::UnixStream;
        let tmpdir = temp_dir();
        declare_service! {
            /// Basic test service
            pub TestService = {
                "sfdjfkosdgjsadgjlas" [| "--liveness" {} |] @ "test-service.sock" as raw |unix_socket| -> Io<UnixStream>  {
                    Ok(unix_socket)
                }
            }
        }

        assert!(block_on(
            TestService
                .reify(&tmpdir)
                .connect(Duration::from_millis(50))
        )
        .is_err());
    }
}

// suss - library for creating single, directory namespaced unix socket servers in a network
// Copyright (C) 2022  Matti Bryce <mattibryce@protonmail.com>

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published
// by the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.

// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
