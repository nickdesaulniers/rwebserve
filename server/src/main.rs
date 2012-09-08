import io;
use io::WriterUtil;
use std::getopts::*;
use std::map::hashmap;
//use rwebserve::IMap::{immutable_map, imap_methods};
use server = rwebserve;

type options = {root: ~str, admin: bool};

// str constants aren't supported yet.
// TODO: get this (somehow) from the link attribute in the rc file (going the other way
// doesn't work because vers in the link attribute has to be a literal)
fn get_version() -> ~str
{
	~"0.1"
}

fn print_usage()
{
	io::println(fmt!("server %s - sample rrest server", get_version()));
	io::println(~"");
	io::println(~"./server [options] --root=<dir>");
	io::println(~"--admin      allows web clients to shut the server down");
	io::println(~"-h, --help   prints this message and exits");
	io::println(~"--root=DIR   path to the directory containing html files");
	io::println(~"--version    prints the server version number and exits");
} 

fn parse_command_line(args: &[~str]) -> options
{
	let opts = ~[
		optflag(~"admin"),
		reqopt(~"root"),
		optflag(~"h"),
		optflag(~"help"),
		optflag(~"version")
	];
	
	let mut t = ~[];
	for vec::eachi(args)		// TODO: tail should work eventually (see https://github.com/mozilla/rust/issues/2770)
	|i, a|
	{
		if i > 0
		{
			vec::push(t, copy(a));
		}
	}
	//let t = vec::tail(args);
	
	let matched = match getopts(t, opts)
	{
		result::Ok(m)	=> copy(m),
		result::Err(f)	=> {io::stderr().write_line(fail_str(f)); libc::exit(1_i32)}
	};
	if opt_present(matched, ~"h") || opt_present(matched, ~"help")
	{
		print_usage();
		libc::exit(0_i32);
	}
	else if opt_present(matched, ~"version")
	{
		io::println(fmt!("server %s", get_version()));
		libc::exit(0_i32);
	}
	else if vec::is_not_empty(matched.free)
	{
		io::stderr().write_line("Positional arguments are not allowed.");
		libc::exit(1_i32);
	}
	{root: opt_str(matched, ~"root"), admin: opt_present(matched, ~"admin")}
}

fn validate_options(options: options)
{
	if !os::path_is_dir(options.root)
	{
		io::stderr().write_line(fmt!("'%s' does not point to a directory.", options.root));
		libc::exit(1_i32);
	}
}

fn process_command_line(args: ~[~str]) -> ~str
{
	if vec::len(args) != 2u || !str::starts_with(args[1], "--root=")
	{
		io::stderr().write_line("Expected a --root-path argument pointing to the html pages.");
		libc::exit(1_i32); 
	}
	
	str::slice(args[1], str::len("--root="), str::len(args[1]))
}

// Like spawn_listener except the new task (and whatever tasks it spawns) are distributed
// among a fixed number of OS threads. TODO: work around for https://github.com/mozilla/rust/issues/2841
fn spawn_threaded_listener<A:send>(num_threads: uint, +block: fn~ (comm::Port<A>)) -> comm::Chan<A>
{
	let channel_port: comm::Port<comm::Chan<A>> = comm::Port();
	let channel_channel = comm::Chan(channel_port);
	
	do task::spawn
	{
		let task_port: comm::Port<A> = comm::Port();
		let task_channel = comm::Chan(task_port);
		comm::send(channel_channel, task_channel);
		
		block(task_port);
	};
	
	comm::recv(channel_port)
}

fn home_view(_settings: hashmap<~str, ~str>, options: options, _request: &server::Request, response: &server::Response) -> server::Response
{
	response.context.insert(~"admin", mustache::Bool(options.admin));
	{template: ~"home.html" , .. response}
}

fn greeting_view(_settings: hashmap<~str, ~str>, request: &server::Request, response: &server::Response) -> server::Response
{
	response.context.insert(~"user-name", mustache::Str(@request.matches.get(~"name")));
	{template: ~"hello.html" , .. response}
}

enum StateMesg
{
	add_listener(~str, comm::Chan<int>),	// str is used to identify the listener
	remove_listener(~str),
	shutdown,
}

type StateChan = comm::Chan<StateMesg>;

// This is a single task that manages the state for our sample server. Normally this will
// do something like get notified of database changes and send messages to connection
// specific listeners. The listeners could then use server-sent events (sse) to push new
// data to the client.
//
// In this case our state is just an int and we notify listeners when we change it.
fn manage_state() -> StateChan
{
	do spawn_threaded_listener(3)
	|state_port: comm::Port<StateMesg>|
	{
		let timer_port = comm::Port();
		let timer_chan = comm::Chan(timer_port);
		
		// TODO: Can get rid of this once peek works better. See https://github.com/mozilla/rust/issues/2841
		do task::spawn
		{
			loop
			{
				libc::funcs::posix88::unistd::sleep(1);
				comm::send(timer_chan, 1);
			}
		};
		
		let mut time = 0;
		let listeners = std::map::str_hash();
		loop
		{
			match comm::select2(timer_port, state_port)
			{
				either::Left(_) =>
				{
					time += 1;
					for listeners.each_value |ch| {comm::send(ch, copy(time))};
				}
				either::Right(add_listener(key, ch)) =>
				{
					let added = listeners.insert(key, ch);
					assert added;
				}
				either::Right(remove_listener(key)) =>
				{
					listeners.remove(key);
				}
				either::Right(shutdown) =>
				{
					break;
				}
			}
		}
	}
}

// Each client connection that hits /uptime will cause an instance of this task to run. When
// manage_state tells us that the world has changed we push the new world (an int in
// this case) out to the client.
fn uptime_sse(registrar: StateChan, request: &server::Request, push: server::PushChan) -> server::ControlChan
{
	let seconds = request.params.get(~"units") == ~"s";
	
	do spawn_threaded_listener(2)
	|ControlPort: server::ControlPort|
	{
		info!("starting uptime sse stream");
		let notify_port = comm::Port();
		let notify_chan = comm::Chan(notify_port);
		
		let key = fmt!("uptime %?", ptr::addr_of(notify_port));
		comm::send(registrar, add_listener(key, notify_chan));
		
		loop
		{
			let mut time = 0;
			match comm::select2(notify_port, ControlPort)
			{
				either::Left(new_time) =>
				{
					// To help test the request code we can push uptimes as
					// seconds or minutes based on a query string.
					if seconds
					{
						time = new_time;
					}
					else
					{
						time = new_time/60;
					}
					comm::send(push, fmt!("retry: 5000\ndata: %?\n\n", time));
				}
				either::Right(server::RefreshEvent) =>
				{
					comm::send(push, fmt!("retry: 5000\ndata: %?\n\n", time));
				}
				either::Right(server::CloseEvent) =>
				{
					info!("shutting down uptime sse stream");
					comm::send(registrar, remove_listener(key));
					break;
				}
			}
		}
	}
}

fn main(args: ~[~str])
{
	let options = parse_command_line(args);
	validate_options(options);
	
	let registrar = manage_state();
	
	// This is an example of how additional information can be communicated to
	// a view handler (in this case we're only communicating options.admin so
	// using settings would be simpler).
	let home: server::ResponseHandler = |settings, request, response| {home_view(settings, options, request, response)};
	let bail: server::ResponseHandler = |_settings, _request, _response|
	{
		info!("received shutdown request");
		libc::exit(0)
	};
	let up: server::OpenSse = |_settings, request, push| {uptime_sse(registrar, request, push)};
	
	let config = {
		hosts: ~[~"localhost", ~"10.6.210.132"],
		port: 8088_u16,
		server_info: ~"sample rrest server " + get_version(),
		resources_root: options.root,
		routes: ~[
			(~"GET", ~"/", ~"home"),
			(~"GET", ~"/shutdown", ~"shutdown"),		// TODO: enable this via debug cfg (or maybe via a command line option)
			(~"GET", ~"/hello/{name}", ~"greeting"),
		],
		views: ~[
			(~"home",  home),
			(~"shutdown",  bail),
			(~"greeting", greeting_view),
		],
		sse: ~[(~"/uptime", up)],
		settings: ~[(~"debug",  ~"true")]
		, .. server::initialize_config()};
	
	server::start(config);
	info!("exiting sample server");		// usually don't land here
}

