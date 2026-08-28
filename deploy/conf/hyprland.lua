-- Functions
local read_file = function(path)
	local file = io.open(path, 'r')
	if not file then
		error('Could not open file: ' .. path)
	end
	local content = file:read('*a')
	file:close()
	return content
end

local function trim(s)
	return s:match('^%s*(.-)%s*$')
end

local function read_orientation(default)
	local file = io.open(os.getenv('HOME') .. '/.config/hypr/orientation', 'r')
	if not file then
		return default
	end
	local n = tonumber(trim(file:read('*a')))
	file:close()
	if n and n >= 0 and n <= 7 then
		return n
	end
	return default
end

-- Variables
local u = os.getenv('HOME') .. '/.config/nushell/u'
local hostname = trim(read_file('/etc/hostname'))
local orientation = read_orientation(hostname == 'armpipa-alx' and 3 or 0)
local terminal = 'kitty'
local menu = 'wofi --show drun'
local mainMod = 'SUPER'

-- TODO: source directive - may need require() or separate handling
-- source = ~/.config/hypr/hypr-monitor.conf

if hostname == 'armpipa-alx' then
	print('Activating armpipa-alx monitors')
	hl.monitor({
		output = 'DP-1',
		mode = '1920x1080',
		position = '0x0',
		scale = 1,
	})

	hl.monitor({
		output = '',
		mode = 'prefered',
		position = 'auto',
		mirror = 'DP-1',
		scale = 1,
	})
	hl.monitor({
		output = 'DSI-1',
		mode = 'preferred',
		position = 'auto',
		mirror = 'DP-1',
		scale = 1,
		transform = orientation,
	})
else
	print('Activating generic monitors')
	hl.monitor({
		output = '',
		mode = 'preferred',
		position = 'auto',
		scale = 1,
		transform = orientation,
	})

	hl.monitor({
		output = '',
		mode = 'preferred',
		position = 'auto',
		mirror = 'eDP-1',
		scale = 1,
	})
end

-- Environment variables
hl.env('XCURSOR_SIZE', '24')
hl.env('HYPRCURSOR_THEME', 'Bibata-Modern-Ice')
hl.env('HYPRCURSOR_SIZE', '24')
hl.env('QT_IM_MODULES', 'wayland;fcitx5;ibus')
hl.env('XMODIFIERS', '@im=fcitx5')
hl.env('GLFW_IM_MODULE', 'fcitx5')
hl.env('UNIPICKER_COPY_COMMAND', 'wl-copy')
-- Kill DXVK's "Compiling shaders..." bottom-left HUD message (its `compiler`
-- item is on by default in some Proton/DXVK builds). DXVK_* vars pass through
-- umu/pressure-vessel, so this reaches every game.
hl.env('DXVK_HUD', '0')

-- Autostart
hl.on('hyprland.start', function()
	hl.exec_cmd('systemctl --user start xdg-desktop-portal-hyprland')
	hl.exec_cmd('hyprpaper')
	hl.exec_cmd('dunst')
	hl.exec_cmd('systemctl --user start mdrv-ds-audio.service') -- overlay mixer (needs Wayland up)
	hl.exec_cmd('pkill -x qs; flock -n /tmp/quickshell.lock qs') -- kill stale instance (stale HIS) + single-instance relaunch
	hl.exec_cmd('otd-daemon')
	hl.exec_cmd('fcitx5 -d -s 2')
	-- mdrv-ds suite (replaces the mdrv-gm frontend): launcher carousel +
	-- settings + audio are systemd user units (enabled via
	-- WantedBy=default.target); started here for symmetry with
	-- mdrv-ds-audio above so a re-run of this start block is a no-op.
	hl.exec_cmd('systemctl --user start mdrv-ds-launcher.service mdrv-ds-settings.service')
	-- hl.exec_cmd('exec steam -silent')
end)

hl.on('hyprland.shutdown', function()
	os.execute('systemctl --user stop xdg-desktop-portal-hyprland')
end)

hl.on('config.reloaded', function()
	hl.exec_cmd('notify-send "Hyprland config reloaded" -t 1000')
end)

-- Permissions
hl.config({
	ecosystem = {
		enforce_permissions = false,
	},
})

-- TODO: verify binary paths match actual system locations
hl.permission({ binary = '^(grim|hyprshot|swaygrab)$', type = 'screencopy', mode = 'allow' })
hl.permission({ binary = '^xdg-desktop-portal-hyprland$', type = 'plugin', mode = 'allow' })
hl.permission({ binary = '^hyprpm$', type = 'keyboard', mode = 'allow' })
hl.permission({ binary = '^wayvnc$', type = 'screencopy', mode = 'allow' })
hl.permission({ binary = '^(hyprlock|hyprctl)$', type = 'cursorpos', mode = 'allow' })
hl.permission({ binary = '^hyprpicker$', type = 'screencopy', mode = 'allow' })

-- General settings
hl.config({
	debug = {
		disable_logs = false,
	},

	general = {
		gaps_in = 5,
		gaps_out = 40,
		border_size = 1,
		col = {
			active_border = { colors = { 'rgba(33ccffee)', 'rgba(00ff99ee)' }, angle = 45 },
			inactive_border = 'rgba(595959aa)',
		},
		resize_on_border = false,
		allow_tearing = false,
		layout = 'dwindle',
	},

	decoration = {
		rounding = 0,
		rounding_power = 2,
		active_opacity = 1.0,
		inactive_opacity = 1.0,
		shadow = {
			enabled = true,
			range = 8,
			render_power = 3,
			color = 'rgba(1a1a1a88)',
		},
		blur = {
			enabled = true,
			size = 1,
			vibrancy = 0,
		},
	},

	animations = {
		enabled = true,
	},

	dwindle = {
		-- pseudotile = true,
		preserve_split = true,
	},

	master = {
		new_status = 'master',
	},

	misc = {
		force_default_wallpaper = 0,
		disable_splash_rendering = true,
		disable_hyprland_logo = true,
	},

	binds = {
		allow_workspace_cycles = false,
	},

	cursor = {
		inactive_timeout = 5,
		no_warps = true,
		no_hardware_cursors = true,
	},

	input = {
		kb_layout = 'us',
		follow_mouse = 1,
		focus_on_close = 1,
		sensitivity = 0.0, -- TODO: was 0.1 in conf but wiki says input.sensitivity is float (verify)
		repeat_rate = 40,
		repeat_delay = 250,
		touchpad = {
			natural_scroll = false,
		},
		accel_profile = 'flat', -- prevent sluggish touchpad movement
		-- force_no_accel = true, -- not needed, atm
	},

	gestures = {
		workspace_swipe_touch = true,
	},
})

-- Devices
hl.device({
	name = 'nvtcapacitivetouchscreen',
	transform = orientation,
})
hl.device({
	name = 'nvtcapacitivepen',
	transform = orientation,
})
hl.device({
	name = 'epic-mouse-v1',
	sensitivity = -0.5,
})

-- Animations: curves
hl.curve('easeOutQuint', { type = 'bezier', points = { { 0.23, 1 }, { 0.32, 1 } } })
hl.curve('easeInOutCubic', { type = 'bezier', points = { { 0.65, 0 }, { 0.35, 1 } } })
hl.curve('linear', { type = 'bezier', points = { { 0, 0 }, { 1, 1 } } })
hl.curve('almostLinear', { type = 'bezier', points = { { 0.5, 0.1 }, { 0.5, 0.1 } } })
hl.curve('quick', { type = 'bezier', points = { { 1.5, 0.7 }, { 1, 0.2 } } })

-- Animation definitions
hl.animation({ leaf = 'windows', enabled = true, speed = 1, bezier = 'easeOutQuint' })
hl.animation({ leaf = 'windowsOut', enabled = true, speed = 1, bezier = 'linear', style = 'popin 80%' })
hl.animation({ leaf = 'windowsMove', enabled = true, speed = 1, bezier = 'easeOutQuint' })
hl.animation({ leaf = 'border', enabled = true, speed = 1, bezier = 'almostLinear' })
hl.animation({ leaf = 'fade', enabled = true, speed = 1, bezier = 'linear' })
hl.animation({ leaf = 'fadeDim', enabled = true, speed = 1, bezier = 'linear' })
hl.animation({ leaf = 'workspaces', enabled = true, speed = 1, bezier = 'easeOutQuint' })
hl.animation({ leaf = 'workspacesIn', enabled = true, speed = 1, bezier = 'easeOutQuint', style = 'slidefade' })
hl.animation({ leaf = 'workspacesOut', enabled = true, speed = 1, bezier = 'easeOutQuint', style = 'slidefade' })
hl.animation({ leaf = 'specialWorkspace', enabled = true, speed = 1, bezier = 'easeOutQuint', style = 'fade' })
hl.animation({ leaf = 'layersIn', enabled = true, speed = 2, bezier = 'easeOutQuint' })
hl.animation({ leaf = 'layersOut', enabled = true, speed = 1, bezier = 'linear' })
hl.animation({ leaf = 'fadeLayersIn', enabled = true, speed = 2, bezier = 'easeOutQuint' })
hl.animation({ leaf = 'fadeLayersOut', enabled = true, speed = 1, bezier = 'linear' })
hl.animation({ leaf = 'borderangle', enabled = true, speed = 3, bezier = 'linear' })
-- hl.animation({ leaf = 'enableActive', enabled = true, speed = 100, bezier = 'linear' })
-- hl.animation({ leaf = 'disableActive', enabled = true, speed = 500, bezier = 'linear' })
hl.animation({ leaf = 'fadeSwitch', enabled = true, speed = 3, bezier = 'easeOutQuint' })
hl.animation({ leaf = 'fadeIn', enabled = true, speed = 3, bezier = 'linear' })
hl.animation({ leaf = 'fadeOut', enabled = true, speed = 3, bezier = 'linear' })
hl.animation({ leaf = 'fadeShadow', enabled = true, speed = 3, bezier = 'linear' })
-- hl.animation({ leaf = 'fadeSplash', enabled = true, speed = 10, bezier = 'linear' })
-- hl.animation({ leaf = 'fadeWindowsIn', enabled = true, speed = 10, bezier = 'linear' })
-- hl.animation({ leaf = 'fadeWindowsOut', enabled = true, speed = 10, bezier = 'linear' })
hl.animation({ leaf = 'monitorAdded', enabled = true, speed = 3, bezier = 'linear' })

-- Window rules
hl.window_rule({
	match = { class = '^hypr-foot-.*$' },
	move = { 'monitor_w * 0.5 - window_w * 0.5', 'monitor_h * 0.5 - window_h * 0.5' },
	float = true,
	-- animation = false,
	size = { 'monitor_w * 0.55', 'monitor_h * 0.65' },
})
hl.window_rule({
	match = { class = '^mpv$' },
	float = true,
})
hl.window_rule({
	match = { class = '^firefox$', title = '^Picture-in-Picture$' },
	float = true,
})
hl.window_rule({
	match = { class = '^org\\.inkscape\\.Inkscape$', title = 'negative:.* - Inkscape$' },
	float = true,
})
hl.window_rule({
	match = { class = '^hypr-ax$' },
	-- opacity = { active = 0.9, inactive = 0.5 },
	border_size = 0,
	float = true,
	size = { '25%', '100%' },
})
hl.window_rule({
	match = { class = '^ueberzugpp_.*$' },
	float = true,
	no_initial_focus = true,
	border_size = 0,
})
hl.window_rule({
	match = { class = '^ibus-ui-gtk3$' },
	float = true,
	no_initial_focus = true,
	border_size = 0,
})
hl.window_rule({
	match = { class = '^Min$' },
	move = { 'monitor_w * 0.5 - window_w * 0.5', 'monitor_h * 0.5 - window_h * 0.5' },
	float = true,
	pin = true,
	-- animation = false,
	size = { 'monitor_w * 0.25', 'monitor_h * 0.75' },
})
hl.window_rule({
	match = { class = '.*' },
	suppress_event = 'maximize',
})
hl.window_rule({
	match = { class = '^(ffxvi.exe)$' },
	immediate = true,
	no_vrr = true,
})

hl.bind(mainMod .. ' + G', hl.dsp.exec_cmd('mdrv-ds-launcher toggle'))
hl.bind(mainMod .. ' + SHIFT + G', hl.dsp.exec_cmd('mdrv-ds-settings toggle'))

-- Keybinds
-- Squeekboard
hl.bind(mainMod .. ' + K', hl.dsp.exec_cmd('nu -n ~/.config/nushell/u/squeekboard.nu'))

-- Monitor toggle
hl.bind(mainMod .. ' + SHIFT + M', hl.dsp.exec_cmd('nu -n ~/.config/hypr/toggle-monitor.nu'))

-- Hyprpicker + copy
hl.bind(mainMod .. ' + SHIFT + P', hl.dsp.exec_cmd('hyprpicker | wl-copy'))

-- Terminal
hl.bind(mainMod .. ' + Return', hl.dsp.exec_cmd(terminal))

-- Kill active
hl.bind(mainMod .. ' + SHIFT + C', hl.dsp.window.close())

-- Exit
hl.bind(mainMod .. ' + CONTROL + SHIFT + Q', hl.dsp.exit())

-- Toggle floating
hl.bind(mainMod .. ' + V', hl.dsp.window.float({ action = 'toggle' }))

-- Menu (wofi)
hl.bind(mainMod .. ' + R', hl.dsp.exec_cmd(menu))

-- Reload config
hl.bind(mainMod .. ' + CONTROL + R', hl.dsp.exec_cmd('hyprctl reload'))

-- Drun menu
hl.bind(mainMod .. ' + P', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-drun.nu'))

-- Window switcher
hl.bind(mainMod .. ' + O', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-window.nu'))
hl.bind(mainMod .. ' + W', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-window.nu'))

-- Toggle split
hl.bind(mainMod .. ' + T', hl.dsp.layout('togglesplit'))

-- Fullscreen
hl.bind(mainMod .. ' + F', hl.dsp.window.fullscreen())

-- Hyprlauncher
hl.bind(mainMod .. ' + E', hl.dsp.exec_cmd('hyprlauncher'))

-- Kitty herdr (default agent multiplexer)
hl.bind(mainMod .. ' + Q', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-kitty.nu -n -i herdr herdr --session k'))

-- Kitty zellij (fallback multiplexer)
hl.bind(mainMod .. ' + SHIFT + Q', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-kitty.nu -n -i zellij zellij a k -c'))

-- Min (special workspace / scratchpad)
hl.bind(mainMod .. ' + S', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-min.nu'))
hl.bind(mainMod .. ' + SHIFT + S', hl.dsp.submap('set'))

-- Shrink/expand area
hl.bind(mainMod .. ' + MINUS', hl.dsp.window.resize({ x = -20, y = -20 }), { repeating = true })
hl.bind(mainMod .. ' + EQUAL', hl.dsp.window.resize({ x = 20, y = 20 }), { repeating = true })

-- Grayscale shade
hl.bind(
	mainMod .. ' + ALT + C',
	hl.dsp.exec_cmd('hyprctl keyword decoration:screen_shader ~/.config/hypr/shaders/grayscale.frag')
)

-- Blue-light filter
hl.bind(
	mainMod .. ' + CONTROL + C',
	hl.dsp.exec_cmd('hyprctl keyword decoration:screen_shader ~/.config/hypr/shaders/blue-light.frag')
)

-- Wallpaper menu
hl.bind(mainMod .. ' + SHIFT + W', hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-wp-menu.nu --default'))
hl.bind(
	mainMod .. ' + CONTROL + SHIFT + W',
	hl.dsp.exec_cmd('nu -n ' .. u .. '/hypr-foot.nu -i wp nu ' .. u .. '/hypr-wp.nu')
)

-- Emoji picker
hl.bind(mainMod .. ' + PERIOD', hl.dsp.exec_cmd('bemoji --noline'))

-- Focus direction (arrows)
hl.bind(mainMod .. ' + Left', hl.dsp.focus({ direction = 'l' }))
hl.bind(mainMod .. ' + Right', hl.dsp.focus({ direction = 'r' }))
hl.bind(mainMod .. ' + Up', hl.dsp.focus({ direction = 'u' }))
hl.bind(mainMod .. ' + Down', hl.dsp.focus({ direction = 'd' }))

-- Focus direction (hjkl)
hl.bind(mainMod .. ' + H', hl.dsp.focus({ direction = 'l' }))
hl.bind(mainMod .. ' + L', hl.dsp.focus({ direction = 'r' }))
hl.bind(mainMod .. ' + K', hl.dsp.focus({ direction = 'u' }))
hl.bind(mainMod .. ' + J', hl.dsp.focus({ direction = 'd' }))

-- Move windows (hjkl)
hl.bind(mainMod .. ' + SHIFT + H', hl.dsp.window.move({ direction = 'l' }))
hl.bind(mainMod .. ' + SHIFT + L', hl.dsp.window.move({ direction = 'r' }))
hl.bind(mainMod .. ' + SHIFT + K', hl.dsp.window.move({ direction = 'u' }))
hl.bind(mainMod .. ' + SHIFT + J', hl.dsp.window.move({ direction = 'd' }))

-- Workspaces 1-5
hl.bind(mainMod .. ' + 1', hl.dsp.focus({ workspace = 1 }))
hl.bind(mainMod .. ' + 2', hl.dsp.focus({ workspace = 2 }))
hl.bind(mainMod .. ' + 3', hl.dsp.focus({ workspace = 3 }))
hl.bind(mainMod .. ' + 4', hl.dsp.focus({ workspace = 4 }))
hl.bind(mainMod .. ' + 5', hl.dsp.focus({ workspace = 5 }))

-- Move to workspaces 1-5
hl.bind(mainMod .. ' + SHIFT + 1', hl.dsp.window.move({ workspace = 1 }))
hl.bind(mainMod .. ' + SHIFT + 2', hl.dsp.window.move({ workspace = 2 }))
hl.bind(mainMod .. ' + SHIFT + 3', hl.dsp.window.move({ workspace = 3 }))
hl.bind(mainMod .. ' + SHIFT + 4', hl.dsp.window.move({ workspace = 4 }))
hl.bind(mainMod .. ' + SHIFT + 5', hl.dsp.window.move({ workspace = 5 }))

-- Workspace +/- (tab)
hl.bind(mainMod .. ' + TAB', hl.dsp.focus({ workspace = '+1' }))
hl.bind(mainMod .. ' + SHIFT + TAB', hl.dsp.focus({ workspace = '-1' }))

-- Mouse scroll workspace
hl.bind('', hl.dsp.focus({ workspace = '+1' }), { mouse = true, mouse_key = 'mouse_down' })
hl.bind('', hl.dsp.focus({ workspace = '-1' }), { mouse = true, mouse_key = 'mouse_up' })

-- Mouse drag/resize
hl.bind(mainMod .. ' + mouse:272', hl.dsp.window.drag(), { mouse = true })
hl.bind(mainMod .. ' + mouse:273', hl.dsp.window.resize(), { mouse = true })

-- Volume (repeating)
hl.bind(
	'',
	hl.dsp.exec_cmd('wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-'),
	{ repeating = true, mouse_key = 'XF86AudioLowerVolume' }
)
hl.bind(
	'',
	hl.dsp.exec_cmd('wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+'),
	{ repeating = true, mouse_key = 'XF86AudioRaiseVolume' }
)
hl.bind('', hl.dsp.exec_cmd('wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle'), { mouse_key = 'XF86AudioMute' })
hl.bind('', hl.dsp.exec_cmd('wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle'), { mouse_key = 'XF86AudioMicMute' })

-- Brightness (repeating)
hl.bind('', hl.dsp.exec_cmd('brightnessctl s 5%-'), { repeating = true, mouse_key = 'XF86MonBrightnessDown' })
hl.bind('', hl.dsp.exec_cmd('brightnessctl s 5%+'), { repeating = true, mouse_key = 'XF86MonBrightnessUp' })
hl.bind('XF86MonBrightnessDown', hl.dsp.exec_cmd('brightnessctl s 5%-'), { repeating = true })
hl.bind('XF86MonBrightnessUp', hl.dsp.exec_cmd('brightnessctl s 5%+'), { repeating = true })

-- Media controls (locked)
hl.bind('', hl.dsp.exec_cmd('playerctl play-pause'), { locked = true, mouse_key = 'XF86AudioPlay' })
hl.bind('', hl.dsp.exec_cmd('playerctl previous'), { locked = true, mouse_key = 'XF86AudioPrev' })
hl.bind('', hl.dsp.exec_cmd('playerctl next'), { locked = true, mouse_key = 'XF86AudioNext' })
hl.bind('', hl.dsp.exec_cmd('playerctl stop'), { locked = true, mouse_key = 'XF86AudioStop' })

-- DPMS toggle
hl.bind(mainMod .. ' + ALT + ESCAPE', hl.dsp.dpms({ action = 'toggle' }))

-- Suspend
hl.bind(mainMod .. ' + ALT + S', hl.dsp.exec_cmd('systemctl suspend'))

-- Lock screen
hl.bind(mainMod .. ' + ALT + L', hl.dsp.exec_cmd('hyprlock'))

-- Bar toggle (qs ipc)
hl.bind(mainMod .. ' + B', hl.dsp.exec_cmd('qs ipc call bar toggle'))
hl.bind(mainMod .. ' + SHIFT + B', hl.dsp.exec_cmd('qs ipc call bar toggle'))

-- String utilities
hl.bind(mainMod .. ' + T', hl.dsp.exec_cmd('nu -n ' .. u .. '/str.nu -t time'))
hl.bind(mainMod .. ' + ALT + T', hl.dsp.exec_cmd('nu -n ' .. u .. '/str.nu -t epoch'))
hl.bind(mainMod .. ' + U', hl.dsp.exec_cmd('nu -n ' .. u .. '/str.nu -t surrealid'))
hl.bind(mainMod .. ' + SHIFT + U', hl.dsp.exec_cmd('nu -n ' .. u .. '/str.nu -t ulid'))

-- MPV pause toggle
hl.bind(mainMod .. ' + COMMA', hl.dsp.exec_cmd('nu -n ' .. u .. '/mpv.nu --ctrl toggle-pause'))
hl.bind(mainMod .. ' + X', hl.dsp.exec_cmd('nu -n ' .. u .. '/mpv.nu --ctrl toggle-pause'))

-- Submap: cursor control
-- ## SUBMAP:cursor
hl.define_submap('cursor', function()
	-- ## A → jump
	-- ## JKLH → move
	-- ## SDF → click
	-- ## TGRE → drag
	-- Jump cursor position via wl-kbptr
	hl.bind('A', hl.dsp.exec_cmd('wl-kbptr jump'))

	-- Move cursor (repeating)
	hl.bind('J', hl.dsp.exec_cmd('wl-kbptr move down 16'), { repeating = true })
	hl.bind('K', hl.dsp.exec_cmd('wl-kbptr move up 16'), { repeating = true })
	hl.bind('L', hl.dsp.exec_cmd('wl-kbptr move right 16'), { repeating = true })
	hl.bind('H', hl.dsp.exec_cmd('wl-kbptr move left 16'), { repeating = true })

	-- Click buttons
	hl.bind('S', hl.dsp.exec_cmd('wl-kbptr click left'))
	hl.bind('D', hl.dsp.exec_cmd('wl-kbptr click right'))
	hl.bind('F', hl.dsp.exec_cmd('wl-kbptr click middle'))

	-- Scroll (repeating)
	hl.bind('E', hl.dsp.exec_cmd('wl-kbptr scroll down 30'), { repeating = true })
	hl.bind('R', hl.dsp.exec_cmd('wl-kbptr scroll up 30'), { repeating = true })
	hl.bind('T', hl.dsp.exec_cmd('wl-kbptr scroll right 30'), { repeating = true })
	hl.bind('G', hl.dsp.exec_cmd('wl-kbptr scroll left 30'), { repeating = true })

	-- Reset submap and restore cursor settings
	hl.bind('Escape', hl.dsp.submap('reset'))
	-- ## SUBMAP_END:cursor
end)

-- Entry point for cursor submap
hl.bind(mainMod .. ' + C', hl.dsp.submap('cursor'))

-- Entry with cursor settings tweak (no_warps=false, inactive_timeout=0)
hl.define_submap('cursor', function()
	-- TODO: this redefines 'cursor' submap - check if Lua allows multiple defines or needs merge
	hl.config({ cursor = { no_warps = false, inactive_timeout = 0 } })

	hl.bind('A', hl.dsp.exec_cmd('wl-kbptr jump'))
	hl.bind('J', hl.dsp.exec_cmd('wl-kbptr move down 16'), { repeating = true })
	hl.bind('K', hl.dsp.exec_cmd('wl-kbptr move up 16'), { repeating = true })
	hl.bind('L', hl.dsp.exec_cmd('wl-kbptr move right 16'), { repeating = true })
	hl.bind('H', hl.dsp.exec_cmd('wl-kbptr move left 16'), { repeating = true })
	hl.bind('S', hl.dsp.exec_cmd('wl-kbptr click left'))
	hl.bind('D', hl.dsp.exec_cmd('wl-kbptr click right'))
	hl.bind('F', hl.dsp.exec_cmd('wl-kbptr click middle'))
	hl.bind('E', hl.dsp.exec_cmd('wl-kbptr scroll down 30'), { repeating = true })
	hl.bind('R', hl.dsp.exec_cmd('wl-kbptr scroll up 30'), { repeating = true })
	hl.bind('T', hl.dsp.exec_cmd('wl-kbptr scroll right 30'), { repeating = true })
	hl.bind('G', hl.dsp.exec_cmd('wl-kbptr scroll left 30'), { repeating = true })

	-- Reset cursor settings on exit
	hl.bind('Escape', function()
		hl.config({ cursor = { no_warps = true, inactive_timeout = 5 } })
		hl.dsp.submap('reset')
	end)
end)
-- Super+G now opens the mdrv-ds-launcher carousel (see bind above).
-- Super+C already enters this cursor submap, so the duplicate Super+G
-- entry is removed.
-- hl.bind(mainMod .. ' + G', hl.dsp.submap('cursor'))

-- Submap: MPV controls
-- ## SUBMAP:mpv
hl.define_submap('mpv', function()
	-- ## H → previous
	-- ## J → vol-
	-- ## K → vol+
	-- ## L → next
	hl.bind('H', hl.dsp.exec_cmd('nu -n ' .. u .. '/mpv.nu --ctrl playlist-prev'))
	hl.bind('L', hl.dsp.exec_cmd('nu -n ' .. u .. '/mpv.nu --ctrl playlist-next'))
	hl.bind('J', hl.dsp.exec_cmd('nu -n ' .. u .. '/mpv.nu --ctrl volume-down'), { repeating = true })
	hl.bind('K', hl.dsp.exec_cmd('nu -n ' .. u .. '/mpv.nu --ctrl volume-up'), { repeating = true })
	hl.bind('Escape', hl.dsp.submap('reset'))
	-- ## SUBMAP_END:mpv
end)
hl.bind(mainMod .. ' + M', hl.dsp.submap('mpv'))

-- Submap: ushot (screenshots)
-- ## SUBMAP:ushot
hl.define_submap('ushot', function()
	-- ## F → fullscreen
	-- ## S → selected area
	-- ## A → active window
	-- ## T → stt (transcribe)
	-- ## H → high (cron)
	-- ## M → medium (cron)
	-- ## L → low (cron)
	hl.bind('F', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu -n ' .. u .. '/hypr-shot.nu screen --notify')
	end)
	hl.bind('S', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu -n ' .. u .. '/hypr-shot.nu area --notify')
	end)
	hl.bind('A', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu -n ' .. u .. '/hypr-shot.nu active --notify')
	end)
	hl.bind('T', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu ' .. u .. '/stt.nu')
	end)
	hl.bind('L', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu ' .. u .. '/shot.nu --toggle --quality low')
	end)
	hl.bind('M', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu ' .. u .. '/shot.nu --toggle')
	end)
	hl.bind('H', function()
		hl.dispatch(hl.dsp.submap('reset'))
		hl.exec_cmd('nu ' .. u .. '/shot.nu --toggle --quality high')
	end)
	hl.bind('Escape', hl.dsp.submap('reset'))
	-- ## SUBMAP_END:ushot
end)
hl.bind(mainMod .. ' + D', hl.dsp.submap('ushot'))

-- Submap: set (settings adjustments)
-- ## SUBMAP:set
-- TODO: May need fixing
hl.define_submap('set', function()
	-- ## ### SET
	-- ## **O** → Opacity
	-- ## **P** → Padding
	-- ## **R** → Rounding
	-- ## SUBMAP:set_opacity
	hl.define_submap('set_opacity', function()
		-- ## ### OPACITY
		-- ## **1–9** → fixed opacity
		-- ## **- (hyphen)** → –0.02
		-- ## **= (equal)** → +0.02
		-- ## **Shift-O** → opaque on
		-- ## **O** → opaque off
		hl.bind('1', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.1'))
		hl.bind('2', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.2'))
		hl.bind('3', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.3'))
		hl.bind('4', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.4'))
		hl.bind('5', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.5'))
		hl.bind('6', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.6'))
		hl.bind('7', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.7'))
		hl.bind('8', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.8'))
		hl.bind('9', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 0.9'))
		hl.bind('0', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity 1.0'))
		hl.bind('MINUS', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity -0.05'), { repeating = true })
		hl.bind('EQUAL', hl.dsp.exec_cmd('hyprctl keyword decoration:active_opacity +0.05'), { repeating = true })
		hl.bind('SHIFT + O', hl.dsp.exec_cmd('hyprctl keyword decoration:opaque true'))
		hl.bind('O', hl.dsp.exec_cmd('hyprctl keyword decoration:opaque false'))
		hl.bind('Q', hl.dsp.submap('reset')) -- back to 'set'
		hl.bind('Escape', hl.dsp.submap('reset')) -- back to root
		-- ## SUBMAP_END
	end)
	hl.bind('O', hl.dsp.submap('set_opacity'))

	-- ## SUBMAP:set_padding
	hl.define_submap('set_padding', function()
		-- ## ### PADDING
		-- ## Use HJKL (without/with Shift) to control each side.
		hl.bind('0', hl.dsp.exec_cmd('hyprctl keyword general:gaps_out 40 ; hyprctl keyword general:gaps_in 5'))
		hl.bind('H', hl.dsp.exec_cmd('hyprctl keyword general:gaps_out +10'), { repeating = true })
		hl.bind('J', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in +10'), { repeating = true })
		hl.bind('K', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in -10'), { repeating = true })
		hl.bind('L', hl.dsp.exec_cmd('hyprctl keyword general:gaps_out -10'), { repeating = true })
		hl.bind('SHIFT + H', hl.dsp.exec_cmd('hyprctl keyword general:gaps_out +10'), { repeating = true })
		hl.bind('SHIFT + J', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in +10'), { repeating = true })
		hl.bind('SHIFT + K', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in -10'), { repeating = true })
		hl.bind('SHIFT + L', hl.dsp.exec_cmd('hyprctl keyword general:gaps_out -10'), { repeating = true })
		hl.bind('Y', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in -10'), { repeating = true })
		hl.bind('U', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in -10'), { repeating = true })
		hl.bind('I', hl.dsp.exec_cmd('hyprctl keyword general:gaps_in -10'), { repeating = true })
		hl.bind('O', hl.dsp.exec_cmd('hyprctl keyword general:gaps_out -10'), { repeating = true })
		hl.bind(
			'EQUAL',
			hl.dsp.exec_cmd('hyprctl keyword general:gaps_out +10 ; hyprctl keyword general:gaps_in +10'),
			{ repeating = true }
		)
		hl.bind(
			'MINUS',
			hl.dsp.exec_cmd('hyprctl keyword general:gaps_out -10 ; hyprctl keyword general:gaps_in -10'),
			{ repeating = true }
		)
		hl.bind('Escape', hl.dsp.submap('reset'))
		-- ## SUBMAP_END
	end)
	hl.bind('P', hl.dsp.submap('set_padding'))

	-- ## SUBMAP:set_rounding
	hl.define_submap('set_rounding', function()
		-- ## ### ROUNDING
		-- ## Use number, hyphen and equal sign keys to control rounding.
		hl.bind('1', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 1'))
		hl.bind('2', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 2'))
		hl.bind('3', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 3'))
		hl.bind('4', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 4'))
		hl.bind('5', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 5'))
		hl.bind('6', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 6'))
		hl.bind('7', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 7'))
		hl.bind('8', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 8'))
		hl.bind('9', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 9'))
		hl.bind('0', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding 0'))
		hl.bind('MINUS', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding -5'), { repeating = true })
		hl.bind('EQUAL', hl.dsp.exec_cmd('hyprctl keyword decoration:rounding +5'), { repeating = true })
		hl.bind('Escape', hl.dsp.submap('reset'))
		-- ## SUBMAP_END
	end)
	hl.bind('R', hl.dsp.submap('set_rounding'))

	hl.bind('Escape', hl.dsp.submap('reset'))
	-- ## SUBMAP_END:set
end)

-- Conditional audio keybinds
-- TODO: conditional logic for $AUDIO env var - Lua if/else needed here
-- if os.getenv("AUDIO") == "pulseaudio" then
--   -- pactl-based binds would go here (currently commented out in original)
-- else
-- Default: wpctl-based audio binds (already defined above as multimedia keys)
-- end
