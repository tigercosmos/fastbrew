# Mirror of Homebrew::Service#to_plist / #to_systemd_unit driven by the internal
# API's service_* fields. Read-only: only generates text on stdout.
$LOAD_PATH.unshift "/opt/homebrew/Library/Homebrew/vendor/bundle/ruby/4.0.0/gems/plist-3.7.2/lib"
require "plist"
require "json"
require "cgi"

PREFIX = "/opt/homebrew"
CELLAR = "/opt/homebrew/Cellar"
HOME   = "/Users/fastbrew"

def replace_placeholders(s)
  s.to_s.gsub("$HOMEBREW_PREFIX", PREFIX).gsub("$HOMEBREW_CELLAR", CELLAR).gsub("/$HOME", HOME)
end

def expand(path)
  p = path.to_s
  p = HOME + p[1..-1].to_s if p.start_with?("~")
  p = File.join(Dir.pwd, p) unless p.start_with?("/")
  # lexical normalization like File.expand_path
  parts = []
  p.split("/").each do |c|
    next if c == "" || c == "."
    if c == ".."
      parts.pop
    else
      parts << c
    end
  end
  "/" + parts.join("/")
end

SOCKET_RE = %r{^(?<type>[a-z]+)://(?<host>.+):(?<port>[0-9]+)$}i

class Svc
  attr_accessor :name, :run, :run_type, :interval, :cron, :keep_alive, :launch_only_once,
                :require_root, :env, :working_dir, :root_dir, :input_path, :log_path,
                :error_log_path, :restart_delay, :throttle_interval, :stop_timeout, :nice,
                :process_type, :macos_legacy_timers, :sockets, :plist_name, :service_name,
                :run_at_load

  def initialize(name, entry)
    @name = name
    @run_at_load = true
    @run_type = :immediate
    @keep_alive = {}
    @env = {}
    @cron = {}
    @sockets = {}
    @launch_only_once = false
    @require_root = false
    @macos_legacy_timers = false
    @plist_name = "homebrew.mxcl.#{name}"
    @service_name = "homebrew.#{name}"

    if (na = entry["service_name_args"])
      @plist_name = na[":macos"] if na[":macos"]
      @service_name = na[":linux"] if na[":linux"]
    end

    cmd = nil
    if (kw = entry["service_run_kwargs"])
      cmd = kw[":macos"] || kw[":linux"]
    elsif (ra = entry["service_run_args"])
      cmd = ra[0]
    end
    cmd = [cmd] if cmd.is_a?(String)
    @run = (cmd || []).map { |a| replace_placeholders(a) }

    (entry["service_args"] || []).each do |(key, value)|
      case key
      when ":run_type"            then @run_type = value.sub(":", "").to_sym
      when ":interval"            then @interval = value
      when ":cron"                then @cron = parse_cron(value)
      when ":keep_alive"          then @keep_alive = value
      when ":launch_only_once"    then @launch_only_once = value
      when ":require_root"        then @require_root = value
      when ":environment_variables"
        value.each { |k, v| @env[k.sub(":", "")] = replace_placeholders(v) }
      when ":working_dir"         then @working_dir = replace_placeholders(value)
      when ":root_dir"            then @root_dir = replace_placeholders(value)
      when ":input_path"          then @input_path = replace_placeholders(value)
      when ":log_path"            then @log_path = replace_placeholders(value)
      when ":error_log_path"      then @error_log_path = replace_placeholders(value)
      when ":restart_delay"       then @restart_delay = value
      when ":throttle_interval"   then @throttle_interval = value
      when ":stop_timeout"        then @stop_timeout = value
      when ":nice"                then @nice = value
      when ":process_type"        then @process_type = value.sub(":", "").to_sym
      when ":macos_legacy_timers" then @macos_legacy_timers = value
      when ":sockets"
        v = value.is_a?(String) ? { "listeners" => value } : value
        v.each do |k, socket_string|
          m = socket_string.match(SOCKET_RE)
          @sockets[k.sub(":", "")] = { host: m[:host], port: m[:port], type: m[:type] }
        end
      end
    end
  end

  def parse_cron(statement)
    parsed = { Month: "*", Day: "*", Weekday: "*", Hour: "*", Minute: "*" }
    case statement
    when "@hourly" then parsed[:Minute] = 0
    when "@daily" then parsed[:Minute] = 0; parsed[:Hour] = 0
    when "@weekly" then parsed[:Minute] = 0; parsed[:Hour] = 0; parsed[:Weekday] = 0
    when "@monthly" then parsed[:Minute] = 0; parsed[:Hour] = 0; parsed[:Day] = 1
    when "@yearly", "@annually"
      parsed[:Minute] = 0; parsed[:Hour] = 0; parsed[:Day] = 1; parsed[:Month] = 1
    else
      parts = statement.split
      raise "bad cron" if parts.length != 5
      [:Minute, :Hour, :Day, :Month, :Weekday].each_with_index do |sel, i|
        parsed[sel] = Integer(parts[i]) if parts[i] != "*"
      end
    end
    parsed
  end

  def command
    @run.map { |a| a.start_with?("~") ? expand(a) : a }
  end

  def keep_alive?
    !@keep_alive.empty? && @keep_alive[":always"] != false
  end

  def present?(v)
    !v.nil? && v != false && v != "" && !(v.respond_to?(:empty?) && v.empty?)
  end

  def to_plist
    base = {
      Label: @plist_name,
      ProgramArguments: command,
      RunAtLoad: @run_at_load == true,
    }
    base[:LaunchOnlyOnce] = @launch_only_once if @launch_only_once == true
    base[:LegacyTimers] = @macos_legacy_timers if @macos_legacy_timers == true
    base[:ExitTimeOut] = @stop_timeout if present?(@stop_timeout)
    base[:TimeOut] = @restart_delay if present?(@restart_delay)
    base[:ThrottleInterval] = @throttle_interval if present?(@throttle_interval)
    base[:ProcessType] = @process_type.to_s.capitalize if present?(@process_type)
    base[:Nice] = @nice if present?(@nice)
    base[:StartInterval] = @interval if present?(@interval) && @run_type == :interval
    base[:WorkingDirectory] = expand(@working_dir) if present?(@working_dir)
    base[:RootDirectory] = expand(@root_dir) if present?(@root_dir)
    base[:StandardInPath] = expand(@input_path) if present?(@input_path)
    base[:StandardOutPath] = expand(@log_path) if present?(@log_path)
    base[:StandardErrorPath] = expand(@error_log_path) if present?(@error_log_path)
    base[:EnvironmentVariables] = @env unless @env.empty?

    if keep_alive?
      if (always = @keep_alive[":always"]) && always != false
        base[:KeepAlive] = always
      elsif @keep_alive.key?(":successful_exit")
        base[:KeepAlive] = { SuccessfulExit: @keep_alive[":successful_exit"] }
      elsif @keep_alive.key?(":crashed")
        base[:KeepAlive] = { Crashed: @keep_alive[":crashed"] }
      elsif @keep_alive.key?(":path") && present?(@keep_alive[":path"])
        base[:KeepAlive] = { PathState: @keep_alive[":path"].to_s }
      end
    end

    unless @sockets.empty?
      base[:Sockets] = {}
      @sockets.each do |n, info|
        base[:Sockets][n] = {
          SockNodeName: info[:host],
          SockServiceName: info[:port],
          SockProtocol: info[:type].upcase,
        }
      end
    end

    if !@cron.empty? && @run_type == :cron
      base[:StartCalendarInterval] = @cron.reject { |_, value| value == "*" }
    end

    base[:LimitLoadToSessionType] = %w[Aqua Background LoginWindow StandardIO System]
    base.to_plist
  end

  def systemd_quote(str)
    result = +"\""
    str.each_char do |char|
      result << case char
                when "\a" then "\\a"
                when "\b" then "\\b"
                when "\f" then "\\f"
                when "\n" then "\\n"
                when "\r" then "\\r"
                when "\t" then "\\t"
                when "\v" then "\\v"
                when "\\" then "\\\\"
                when "\"" then "\\\""
                else char
                end
    end
    result << "\""
  end

  def to_systemd_unit
    cmd = command.map { |a| systemd_quote(a) }.join(" ")
    options = []
    options << "Type=#{(@launch_only_once == true) ? "oneshot" : "simple"}"
    options << "ExecStart=#{cmd}"
    if !@keep_alive.empty?
      if present?(@keep_alive[":always"]) || present?(@keep_alive[":crashed"])
        options << "Restart=on-failure"
      elsif present?(@keep_alive[":successful_exit"])
        options << "Restart=on-success"
      end
    end
    options << "RestartSec=#{@restart_delay}" if present?(@restart_delay)
    options << "TimeoutStopSec=#{@stop_timeout}" if present?(@stop_timeout)
    options << "Nice=#{@nice}" if present?(@nice)
    options << "WorkingDirectory=#{expand(@working_dir)}" if present?(@working_dir)
    options << "RootDirectory=#{expand(@root_dir)}" if present?(@root_dir)
    options << "StandardInput=file:#{expand(@input_path)}" if present?(@input_path)
    options << "StandardOutput=append:#{expand(@log_path)}" if present?(@log_path)
    options << "StandardError=append:#{expand(@error_log_path)}" if present?(@error_log_path)
    @env.each { |k, v| options << "Environment=\"#{k}=#{v}\"" }

    <<~SYSTEMD
      [Unit]
      Description=Homebrew generated unit for #{@name}

      [Install]
      WantedBy=default.target

      [Service]
      #{options.join("\n")}
    SYSTEMD
  end
end

entries = JSON.parse(File.read(ARGV[0]))
out = {}
entries.each do |name, entry|
  svc = Svc.new(name, entry)
  rec = entry.dup
  next if svc.run.empty?
  rec["expected_plist"] = svc.to_plist
  rec["expected_systemd"] = svc.to_systemd_unit
  rec["expected_label"] = svc.plist_name
  out[name] = rec
end
print JSON.pretty_generate(out)
