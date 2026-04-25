use strict;
use warnings;
use Irssi;
use Time::Local qw(timegm);
use vars qw($VERSION %IRSSI);

$VERSION = '0.6.0';
%IRSSI = (
    authors     => 'matrirc',
    name        => 'matrirc-media',
    description => 'Track matrirc media attachments — /mediashow, /mediasave, /medialist',
    license     => 'GPL-3.0-or-later',
);

my $CURL    = 'curl';
my $OPENER  = $ENV{MATRIRC_IMG_OPEN} // 'open';
my $TMP_DIR = "$ENV{HOME}/.cache/matrirc";
my $SAVE_DIR = $ENV{MATRIRC_SAVE_DIR} // "$ENV{HOME}/Downloads";
my $HISTORY = 30;

# Media kinds matrirc emits (see src/matrix.rs::msgtype_body).
my @KINDS = qw(image video audio file);
my $KIND_RE = '(?:' . join('|', @KINDS) . ')';

mkdir $TMP_DIR unless -d $TMP_DIR;

# Most-recent first: { url, nick, name, kind, target, servtag }
my @recent;

sub track {
    my ($info) = @_;
    unshift @recent, $info;
    splice @recent, $HISTORY if @recent > $HISTORY;
}

sub sniff_ext {
    my $path = shift;
    open(my $fh, '<:raw', $path) or return '';
    read($fh, my $buf, 16);
    close $fh;
    return '.png'  if substr($buf, 0, 8) eq "\x89PNG\r\n\x1a\n";
    return '.jpg'  if substr($buf, 0, 3) eq "\xff\xd8\xff";
    return '.gif'  if $buf =~ /^GIF8[79]a/;
    return '.webp' if length($buf) >= 12 && substr($buf, 8, 4) eq 'WEBP';
    return '.mp4'  if length($buf) >= 8 && substr($buf, 4, 4) eq 'ftyp';
    return '.webm' if substr($buf, 0, 4) eq "\x1a\x45\xdf\xa3";
    return '.mp3'  if substr($buf, 0, 3) eq "ID3" || substr($buf, 0, 2) eq "\xff\xfb";
    return '.ogg'  if substr($buf, 0, 4) eq "OggS";
    return '.pdf'  if substr($buf, 0, 4) eq "%PDF";
    return '.zip'  if substr($buf, 0, 4) eq "PK\x03\x04";
    return '';
}

sub safe_name {
    my $s = shift // '';
    $s =~ s{[/\\\x00-\x1f]}{_}g;
    $s =~ s/^\s+|\s+$//g;
    $s = substr($s, 0, 120);
    return $s;
}

sub urlencode {
    my $s = shift // '';
    $s =~ s/([^A-Za-z0-9._~-])/sprintf('%%%02X', ord($1))/ge;
    return $s;
}

sub origin_for {
    my ($servtag, $target) = @_;
    return undef unless defined $servtag && defined $target;
    my $server = Irssi::server_find_tag($servtag);
    return undef unless $server;
    my $item = ($target =~ /^[#&!+]/) ? $server->channel_find($target)
                                      : $server->query_find($target);
    return undef unless $item;
    return $item->can('window') ? $item->window : $item;
}

sub print_to_origin {
    my ($info, $line) = @_;
    return unless $info && $info->{servtag};
    my $server = Irssi::server_find_tag($info->{servtag});
    return unless $server;
    my $key  = $info->{target};
    my $item = ($key =~ /^[#&!+]/) ? $server->channel_find($key)
                                   : $server->query_find($key);
    $item ||= $server->query_find($info->{nick});
    return unless $item;
    my $win = $item->can('window') ? $item->window : $item;
    return unless $win;
    $win->print($line, Irssi::MSGLEVEL_CLIENTCRAP);
}

sub on_privmsg_event {
    my ($server, $data, $nick, $address) = @_;
    return unless defined $address && $address =~ /\@matrix$/;
    return unless $data =~ /\[($KIND_RE)\]\s*(.*?)\s*<(https?:\/\/[^>\s]+)/;
    my ($kind, $name, $url) = ($1, $2, $3);
    $name =~ s/^\s+|\s+$//g;
    my ($target) = $data =~ /^(\S+)/;
    return unless defined $target;

    my $own = $server && $server->{nick};
    my $lookup = (defined $own && $target eq $own) ? $nick : $target;

    my $iso = $server->meta_stash_find("time");
    my $when = parse_iso($iso) // time;

    my $info = {
        url     => $url,
        nick    => $nick,
        name    => $name,
        kind    => $kind,
        target  => $lookup,
        servtag => $server->{tag},
        time    => $when,
    };
    track($info);
    my $tag = length($name) ? " — $name" : '';
    print_to_origin($info, "↳ $kind #1 from $nick$tag  (/mediashow [N|name])");
}

sub recent_in {
    my ($witem) = @_;
    my $name = $witem ? ($witem->{name} // '') : '';
    return @recent unless length $name;
    return grep { ($_->{target} // '') eq $name } @recent;
}

sub pick {
    my ($spec, $witem) = @_;
    return pick_in(scalar recent_in($witem), $spec);
}

sub pick_in {
    my ($list_or_scope, $spec) = @_;
    $spec //= '';
    my @list = ref($list_or_scope) eq 'ARRAY'
        ? @$list_or_scope
        : grep { ($_->{target} // '') eq $list_or_scope } @recent;
    return $list[0] if $spec eq '';
    return $list[int($spec) - 1] if $spec =~ /^\d+$/;
    my $needle = lc $spec;
    # exact nick match wins (most specific)
    for my $r (@list) {
        return $r if lc($r->{nick} // '') eq $needle;
    }
    # then nick substring
    for my $r (@list) {
        return $r if lc($r->{nick} // '') =~ /\Q$needle\E/;
    }
    # finally filename substring
    for my $r (@list) {
        return $r if lc($r->{name} // '') =~ /\Q$needle\E/;
    }
    return undef;
}

# Returns ($scope, $remaining_args) — first arg starting with # & ! + is
# treated as an explicit channel scope; otherwise scope is the current
# window's name (or undef for non-window contexts).
sub scope_from_args {
    my ($args, $witem) = @_;
    $args //= '';
    $args =~ s/^\s+|\s+$//g;
    my @parts = split /\s+/, $args, 2;
    if (@parts && $parts[0] =~ /^[#&!+]/) {
        return ($parts[0], $parts[1] // '');
    }
    my $name = $witem ? ($witem->{name} // '') : '';
    return (length $name ? $name : undef, $args);
}

my @MONTHS = qw(Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec);

sub fmt_time {
    my $t = shift // 0;
    my @lt = localtime $t;
    my @today = localtime;
    my $today =
        $lt[5] == $today[5] && $lt[4] == $today[4] && $lt[3] == $today[3];
    return $today
        ? sprintf("%02d:%02d", $lt[2], $lt[1])
        : sprintf("%s %02d %02d:%02d", $MONTHS[$lt[4]], $lt[3], $lt[2], $lt[1]);
}

sub parse_iso {
    my $s = shift;
    return undef unless defined $s && length $s;
    if ($s =~ /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})/) {
        return eval { timegm($6, $5, $4, $3, $2 - 1, $1 - 1900) };
    }
    return undef;
}

# Async-fetch the picked attachment to a final filesystem path. After fetch,
# call $on_done->($info, $final_path) (in parent) via pidwait.
my %pending;

sub fetch_async {
    my (%args) = @_;
    my $info     = $args{info};
    my $dest_dir = $args{dest_dir};
    my $on_done  = $args{on_done};

    my $stamp = time . '-' . $$ . '-' . int(rand(1_000_000));
    my $tmp   = "$TMP_DIR/fetch-$stamp";

    my $pid = fork();
    if (!defined $pid) {
        Irssi::print("matrirc media: fork failed: $!");
        return;
    }
    if ($pid == 0) {
        open(STDIN,  '<', '/dev/null');
        open(STDOUT, '>', '/dev/null');
        open(STDERR, '>', '/dev/null');
        if (system($CURL, '-sfL', '--max-time', '30', '-o', $tmp, $info->{url}) != 0) {
            unlink $tmp;
            exit 2;
        }
        exit 0;
    }

    $pending{$pid} = {
        info    => $info,
        tmp     => $tmp,
        dest_dir => $dest_dir,
        on_done => $on_done,
    };
    Irssi::pidwait_add($pid);
}

sub fork_curl_post {
    my (%args) = @_;
    my $url   = $args{url};
    my $path  = $args{path};
    my $witem = $args{witem};

    my $stamp = time . '-' . $$ . '-' . int(rand(1_000_000));
    my $out   = "$TMP_DIR/upload-$stamp.out";

    my $pid = fork();
    if (!defined $pid) {
        Irssi::print("matrirc media: fork failed: $!");
        return;
    }
    if ($pid == 0) {
        open(STDIN,  '<', '/dev/null');
        open(STDOUT, '>', $out);
        open(STDERR, '>>', $out);
        exec($CURL, '--max-time', '120', '-sS', '-X', 'POST',
             '--data-binary', "\@$path",
             '-H', 'Content-Type: application/octet-stream',
             '-w', "\n%{http_code}\n",
             $url) or exit 9;
    }

    $pending{$pid} = {
        kind    => 'upload',
        out     => $out,
        path    => $path,
        servtag => $witem ? $witem->{server}{tag} : undef,
        target  => $witem ? $witem->{name} : undef,
    };
    Irssi::pidwait_add($pid);
}

Irssi::signal_add('pidwait' => sub {
    my ($pid, $status) = @_;
    my $job = delete $pending{$pid};
    return unless $job;

    if (($job->{kind} // '') eq 'upload') {
        my $body = '';
        if (open(my $fh, '<', $job->{out})) {
            local $/; $body = <$fh>; close $fh;
        }
        unlink $job->{out};

        my @lines = split /\n/, ($body // '');
        while (@lines && $lines[-1] eq '') { pop @lines; }
        my $code = '';
        if (@lines && $lines[-1] =~ /^\d{3}$/) { $code = pop @lines; }
        my $msg_body = join("\n", @lines);

        my $where = origin_for($job->{servtag}, $job->{target});
        if ($code eq '200') {
            return; # silent — sync echo paints the line
        }
        my $line = ($code =~ /^\d{3}$/)
            ? "mediasend: HTTP $code: $msg_body"
            : "mediasend: upload failed (curl exit "
              . ($status >> 8) . "): $msg_body";
        if ($where) { $where->print($line, Irssi::MSGLEVEL_CLIENTCRAP); }
        else        { Irssi::print($line); }
        return;
    }

    if (!-s $job->{tmp}) {
        unlink $job->{tmp};
        Irssi::print("matrirc media: download failed (exit "
            . ($status >> 8) . ") for " . $job->{info}{url});
        return;
    }

    my $ext = sniff_ext($job->{tmp});
    my $base = safe_name($job->{info}{name});
    if (!length $base) {
        $base = $job->{info}{kind} . '-' . time;
    }
    if ($ext && $base !~ /\.[A-Za-z0-9]{2,5}$/) {
        $base .= $ext;
    }

    mkdir $job->{dest_dir} unless -d $job->{dest_dir};
    my $dest = "$job->{dest_dir}/$base";
    if (-e $dest) {
        $dest = "$job->{dest_dir}/" . time . "-$base";
    }
    if (!rename $job->{tmp}, $dest) {
        unlink $job->{tmp};
        Irssi::print("matrirc media: rename to $dest failed: $!");
        return;
    }
    $job->{on_done}->($job->{info}, $dest);
});

sub cmd_mediashow {
    my ($args, $server, $witem) = @_;
    my ($scope, $spec) = scope_from_args($args, $witem);
    if (!defined $scope) {
        Irssi::print("mediashow: usage: /mediashow [#channel] [N|name]");
        return;
    }
    my $info = pick_in($scope, $spec);
    if (!$info) {
        Irssi::print("mediashow: no match in $scope");
        return;
    }
    fetch_async(
        info     => $info,
        dest_dir => $TMP_DIR,
        on_done  => sub {
            my ($info, $path) = @_;
            my $pid = fork();
            return unless defined $pid;
            if ($pid == 0) {
                open(STDIN,  '<', '/dev/null');
                open(STDOUT, '>', '/dev/null');
                open(STDERR, '>', '/dev/null');
                exec($OPENER, $path) or exit 4;
            }
            Irssi::print("mediashow: opened $path");
        },
    );
    my $tag = length($info->{name}) ? $info->{name} : $info->{url};
    Irssi::print("mediashow: fetching $info->{kind} from $info->{nick} in $info->{target}: $tag");
}

sub cmd_mediasave {
    my ($args, $server, $witem) = @_;
    my ($scope, $rest) = scope_from_args($args, $witem);
    if (!defined $scope) {
        Irssi::print("mediasave: usage: /mediasave [#channel] [N|name] [dir]");
        return;
    }
    my @p = split /\s+/, $rest, 2;
    my $spec = $p[0] // '';
    my $dest_dir = $p[1] // $SAVE_DIR;
    $dest_dir =~ s{^~}{$ENV{HOME}};
    my $info = pick_in($scope, $spec);
    if (!$info) {
        Irssi::print("mediasave: no match in $scope");
        return;
    }
    fetch_async(
        info     => $info,
        dest_dir => $dest_dir,
        on_done  => sub {
            my ($info, $path) = @_;
            Irssi::print("mediasave: saved $path");
        },
    );
    my $tag = length($info->{name}) ? $info->{name} : $info->{url};
    Irssi::print("mediasave: fetching $info->{kind} from $info->{nick}: $tag → $dest_dir");
}

sub cmd_medialist {
    my ($args, $server, $witem) = @_;
    my $all = (defined $args && $args =~ /\ball\b/i);
    my @list = $all ? @recent : recent_in($witem);
    if (!@list) {
        Irssi::print("medialist: nothing here yet");
        return;
    }
    my $scope = $all ? 'all channels'
              : ($witem && $witem->{name}) ? "in $witem->{name}" : 'all channels';
    Irssi::print("matrirc media ($scope, most recent first):");
    for my $i (0 .. $#list) {
        my $r = $list[$i];
        Irssi::print(sprintf("  %2d. %-12s [%s] %-12s %-25s %s",
            $i + 1,
            fmt_time($r->{time}),
            $r->{kind},
            $r->{nick},
            $r->{target},
            length($r->{name}) ? $r->{name} : $r->{url}));
    }
}

sub cmd_mediasend {
    my ($args, $server, $witem) = @_;
    if (!$witem || !defined $witem->{name} || $witem->{name} eq '') {
        Irssi::print("mediasend: open a channel/query first");
        return;
    }
    $args //= '';
    $args =~ s/^\s+//;
    my ($path, $caption) = split /\s+/, $args, 2;
    if (!defined $path || $path eq '') {
        Irssi::print("mediasend: usage: /mediasend <path> [caption]");
        return;
    }
    $path =~ s{^~}{$ENV{HOME}};
    if (!-f $path) {
        Irssi::print("mediasend: no such file: $path");
        return;
    }

    my $base = $path;
    $base =~ s{^.*/}{};

    my $url = sprintf('http://127.0.0.1:6680/upload/%s?filename=%s',
                      urlencode($witem->{name}),
                      urlencode($base));
    if (defined $caption && length $caption) {
        $url .= '&caption=' . urlencode($caption);
    }

    fork_curl_post(url => $url, path => $path, witem => $witem);
    Irssi::print("mediasend: uploading $base → $witem->{name}");
}

Irssi::signal_add('event privmsg' => \&on_privmsg_event);
Irssi::command_bind('mediashow' => \&cmd_mediashow);
Irssi::command_bind('mediasave' => \&cmd_mediasave);
Irssi::command_bind('medialist' => \&cmd_medialist);
Irssi::command_bind('mediasend' => \&cmd_mediasend);

Irssi::print("matrirc-media $VERSION loaded; /mediashow, /mediasave, /medialist, /mediasend");
