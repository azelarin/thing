export type ThingStateValue = nil | boolean | number | string

export type ThingState<T> = {
    get: (self: ThingState<T>) -> T,
    set: (self: ThingState<T>, value: T) -> (),
}

export type ThingSignal<T> = {
    get: (self: ThingSignal<T>) -> T,
}

declare extern type ThingTimer with
    read start: (self: ThingTimer) -> ()
    read stop: (self: ThingTimer) -> ()
    read restart: (self: ThingTimer) -> ()
end

export type ThingTimerOptions = {
    interval: number,
    on_timeout: (timer: ThingTimer) -> (),
    repeating: boolean?,
    autostart: boolean?,
}

export type ThingReadonlyArray<T> = { T }

export type ThingReactive<T> = T | (() -> T)

declare extern type ThingWidget with end
declare extern type ThingImageSource with end
declare extern type ThingNumberTransition with end
declare extern type ThingColorTransition with end
declare extern type ThingNumberSpring with end

export type ThingEdge = "top" | "right" | "bottom" | "left"
export type ThingLayer = "background" | "bottom" | "top" | "overlay"
export type ThingKeyboardMode = "none" | "on_demand" | "exclusive"
export type ThingCrossAlign = "start" | "center" | "end" | "stretch"
export type ThingMainAlign =
    "start"
    | "center"
    | "end"
    | "space_between"
    | "space_around"
    | "space_evenly"
export type ThingTextAlign = "start" | "center" | "end"
export type ThingTextOverflow = "clip" | "ellipsis"
export type ThingImageFit = "contain" | "cover" | "fill"
export type ThingPathFit = "contain" | "cover" | "fill"
export type ThingPathFillRule = "nonzero" | "evenodd"
export type ThingPathStrokeCap = "butt" | "round" | "square"
export type ThingPathStrokeJoin = "miter" | "round" | "bevel"
export type ThingPopupAnchor = "auto" | ThingEdge
export type ThingPopupAlignment = "start" | "center" | "end"
export type ThingEasing = "linear" | "in_cubic" | "out_cubic" | "in_out_cubic"
export type ThingManagedProcessStatus =
    "idle" | "starting" | "running" | "stopping" | "exited" | "failed"

export type ThingManagedProcessOptions = {
    key: string,
    shutdown_signal: ("terminate" | "interrupt")?,
}

export type ThingScheduledProcessOptions = {
    key: string,
    command: { string },
    working_directory: string?,
    interval: number?,
    shutdown_signal: ("terminate" | "interrupt")?,
}

export type ThingPointerPhase = "press" | "move" | "release" | "cancel"
export type ThingPointerButton = "primary" | "middle" | "secondary"
export type ThingScrollSource = "wheel" | "finger" | "continuous" | "wheel_tilt" | "unknown"

declare extern type ThingPointerEvent with
    read phase: ThingPointerPhase
    read button: ThingPointerButton
    read x: number
    read y: number
    read surface_x: number
    read surface_y: number
    read shift: boolean
    read ctrl: boolean
    read alt: boolean
    read logo: boolean
end

declare extern type ThingScrollEvent with
    read local_x: number
    read local_y: number
    read surface_x: number
    read surface_y: number
    read shift: boolean
    read ctrl: boolean
    read alt: boolean
    read logo: boolean
    read source: ThingScrollSource
end

export type ThingScrollHandler = (
    x: number,
    y: number,
    event: ThingScrollEvent
) -> ...boolean?

export type ThingTransitionOptions = {
    duration: number,
    easing: ThingEasing?,
}

export type ThingSpringOptions = {
    stiffness: number,
    damping: number,
    epsilon: number?,
}

export type ThingAnchors = {
    top: boolean?,
    right: boolean?,
    bottom: boolean?,
    left: boolean?,
}

export type ThingMargins = {
    top: number?,
    right: number?,
    bottom: number?,
    left: number?,
}

export type ThingInputRectangle = {
    x: number?,
    y: number?,
    width: number?,
    height: number?,
}

export type ThingInputRegion =
    false
    | ThingInputRectangle
    | { rectangles: { ThingInputRectangle } }

export type ThingPathViewBox = {
    x: number,
    y: number,
    width: number,
    height: number,
}

export type ThingPopupOffset = {
    x: number?,
    y: number?,
}

export type ThingPopupConstraints = {
    flip_x: boolean?,
    flip_y: boolean?,
    slide_x: boolean?,
    slide_y: boolean?,
}

export type ThingTransformOrigin = {
    x: number?,
    y: number?,
    offset_x: number?,
    offset_y: number?,
}

type ThingAnimatedNumber = number | (() -> number) | ThingNumberTransition | ThingNumberSpring
type ThingAnimatedOptionalNumber = number | (() -> number?) | ThingNumberTransition | ThingNumberSpring
type ThingAnimatedColor = string | (() -> string) | ThingColorTransition
type ThingImageSourceValue = string | ThingImageSource

export type ThingStyle = {
    width: ThingAnimatedOptionalNumber?,
    height: ThingAnimatedOptionalNumber?,
    width_fraction: ThingReactive<number?>?,
    height_fraction: ThingReactive<number?>?,
    min_width: ThingReactive<number?>?,
    max_width: ThingReactive<number?>?,
    min_height: ThingReactive<number?>?,
    max_height: ThingReactive<number?>?,
    padding: ThingAnimatedNumber?,
    padding_x: ThingAnimatedOptionalNumber?,
    padding_y: ThingAnimatedOptionalNumber?,
    gap: ThingAnimatedNumber?,
    grow: ThingAnimatedNumber?,
    shrink: ThingReactive<number?>?,
    align: ThingReactive<ThingCrossAlign>?,
    align_self: ThingReactive<ThingCrossAlign?>?,
    align_content: ThingReactive<ThingCrossAlign>?,
    justify: ThingReactive<ThingMainAlign>?,
    wrap: ThingReactive<boolean>?,
    radius: ThingAnimatedNumber?,
    border_width: ThingAnimatedNumber?,
    border_color: ThingAnimatedColor?,
    hover_overlay: ThingAnimatedColor?,
    pressed_overlay: ThingAnimatedColor?,
    disabled_opacity: ThingReactive<number>?,
    focus_ring_color: ThingAnimatedColor?,
    focus_ring_width: ThingReactive<number>?,
    font_size: ThingReactive<number>?,
    font_weight: ThingReactive<number>?,
    font_family: ThingReactive<string>?,
    background: ThingAnimatedColor?,
    foreground: ThingAnimatedColor?,
    opacity: ThingAnimatedNumber?,
    translate_x: ThingAnimatedNumber?,
    translate_y: ThingAnimatedNumber?,
    scale_x: ThingAnimatedNumber?,
    scale_y: ThingAnimatedNumber?,
    rotation: ThingAnimatedNumber?,
    transform_origin: ThingReactive<ThingTransformOrigin>?,
}

export type ThingTextStyle = {
    width: ThingAnimatedOptionalNumber?,
    height: ThingAnimatedOptionalNumber?,
    width_fraction: ThingReactive<number?>?,
    height_fraction: ThingReactive<number?>?,
    min_width: ThingReactive<number?>?,
    max_width: ThingReactive<number?>?,
    min_height: ThingReactive<number?>?,
    max_height: ThingReactive<number?>?,
    padding: ThingAnimatedNumber?,
    padding_x: ThingAnimatedOptionalNumber?,
    padding_y: ThingAnimatedOptionalNumber?,
    gap: ThingAnimatedNumber?,
    grow: ThingAnimatedNumber?,
    shrink: ThingReactive<number?>?,
    align: ThingReactive<ThingCrossAlign>?,
    align_self: ThingReactive<ThingCrossAlign?>?,
    align_content: ThingReactive<ThingCrossAlign>?,
    justify: ThingReactive<ThingMainAlign>?,
    wrap: ThingReactive<boolean>?,
    radius: ThingAnimatedNumber?,
    border_width: ThingAnimatedNumber?,
    border_color: ThingAnimatedColor?,
    hover_overlay: ThingAnimatedColor?,
    pressed_overlay: ThingAnimatedColor?,
    disabled_opacity: ThingReactive<number>?,
    focus_ring_color: ThingAnimatedColor?,
    focus_ring_width: ThingReactive<number>?,
    font_size: ThingReactive<number>?,
    font_weight: ThingReactive<number>?,
    font_family: ThingReactive<string>?,
    text_align: ThingReactive<ThingTextAlign>?,
    tabular_digits: ThingReactive<boolean>?,
    background: ThingAnimatedColor?,
    foreground: ThingAnimatedColor?,
    opacity: ThingAnimatedNumber?,
    translate_x: ThingAnimatedNumber?,
    translate_y: ThingAnimatedNumber?,
    scale_x: ThingAnimatedNumber?,
    scale_y: ThingAnimatedNumber?,
    rotation: ThingAnimatedNumber?,
    transform_origin: ThingReactive<ThingTransformOrigin>?,
}

export type ThingTextInputStyle = {
    width: ThingAnimatedOptionalNumber?,
    height: ThingAnimatedOptionalNumber?,
    width_fraction: ThingReactive<number?>?,
    height_fraction: ThingReactive<number?>?,
    min_width: ThingReactive<number?>?,
    max_width: ThingReactive<number?>?,
    min_height: ThingReactive<number?>?,
    max_height: ThingReactive<number?>?,
    padding: ThingAnimatedNumber?,
    padding_x: ThingAnimatedOptionalNumber?,
    padding_y: ThingAnimatedOptionalNumber?,
    gap: ThingAnimatedNumber?,
    grow: ThingAnimatedNumber?,
    shrink: ThingReactive<number?>?,
    align: ThingReactive<ThingCrossAlign>?,
    align_self: ThingReactive<ThingCrossAlign?>?,
    align_content: ThingReactive<ThingCrossAlign>?,
    justify: ThingReactive<ThingMainAlign>?,
    wrap: ThingReactive<boolean>?,
    radius: ThingAnimatedNumber?,
    border_width: ThingAnimatedNumber?,
    border_color: ThingAnimatedColor?,
    hover_overlay: ThingAnimatedColor?,
    pressed_overlay: ThingAnimatedColor?,
    disabled_opacity: ThingReactive<number>?,
    focus_ring_color: ThingAnimatedColor?,
    focus_ring_width: ThingReactive<number>?,
    font_size: ThingReactive<number>?,
    font_weight: ThingReactive<number>?,
    font_family: ThingReactive<string>?,
    text_align: ThingReactive<ThingTextAlign>?,
    tabular_digits: ThingReactive<boolean>?,
    placeholder_color: ThingReactive<string>?,
    selection_color: ThingReactive<string>?,
    caret_color: ThingReactive<string>?,
    background: ThingAnimatedColor?,
    foreground: ThingAnimatedColor?,
    opacity: ThingAnimatedNumber?,
    translate_x: ThingAnimatedNumber?,
    translate_y: ThingAnimatedNumber?,
    scale_x: ThingAnimatedNumber?,
    scale_y: ThingAnimatedNumber?,
    rotation: ThingAnimatedNumber?,
    transform_origin: ThingReactive<ThingTransformOrigin>?,
}

export type ThingChildren = { ThingWidget } | (() -> { ThingWidget })

declare extern type ThingKeyEvent with
    read key: string
    read text: string?
    read shift: boolean
    read ctrl: boolean
    read alt: boolean
    read logo: boolean
    read repeated: boolean
end

export type ThingPanelProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    namespace: string?,
    edge: ThingReactive<ThingEdge>?,
    anchors: ThingReactive<ThingAnchors?>?,
    layer: ThingReactive<ThingLayer>?,
    keyboard_mode: ThingReactive<ThingKeyboardMode>?,
    margins: ThingReactive<ThingMargins>?,
    exclusive_zone: ThingReactive<number?>?,
    input_region: ThingReactive<ThingInputRegion?>?,
    idle_inhibit: ThingReactive<boolean>?,
    on_key: ((event: ThingKeyEvent) -> boolean)?,
    children: ThingChildren?,
}

export type ThingLockProperties = {
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    on_key: ((event: ThingKeyEvent) -> boolean)?,
    children: ThingChildren?,
}

export type ThingContainerProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    children: ThingChildren?,
}

export type ThingTextProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingTextStyle?,
    text: ThingReactive<string>?,
    max_lines: ThingReactive<number?>?,
    overflow: ThingReactive<ThingTextOverflow>?,
}

export type ThingTextInputProperties = {
    key: string?,
    autofocus: boolean?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingTextInputStyle?,
    value: ThingReactive<string>,
    placeholder: ThingReactive<string>?,
    on_change: (value: string) -> (),
    on_submit: ((value: string) -> ())?,
    on_hover: ((inside: boolean) -> ())?,
}

export type ThingSecretInputProperties = {
    key: string?,
    autofocus: boolean?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingTextInputStyle?,
    placeholder: ThingReactive<string>?,
    on_hover: ((inside: boolean) -> ())?,
}

export type ThingImageProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    source: ThingReactive<ThingImageSourceValue?>?,
    icon: ThingReactive<string?>?,
    app_id: ThingReactive<string?>?,
    icon_theme: ThingReactive<string>?,
    fit: ThingReactive<ThingImageFit>?,
}

export type ThingPathProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    data: ThingReactive<string>,
    view_box: ThingReactive<ThingPathViewBox>,
    fit: ThingReactive<ThingPathFit>?,
    fill: ThingReactive<string?>?,
    fill_rule: ThingReactive<ThingPathFillRule>?,
    stroke: ThingReactive<string>?,
    stroke_width: ThingReactive<number>?,
    stroke_cap: ThingReactive<ThingPathStrokeCap>?,
    stroke_join: ThingReactive<ThingPathStrokeJoin>?,
}

export type ThingButtonProperties = {
    key: string?,
    autofocus: boolean?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    children: ThingChildren?,
    on_click: (() -> ())?,
    on_middle_click: (() -> ())?,
    on_secondary_click: (() -> ())?,
    on_scroll: ThingScrollHandler?,
    on_hover: ((inside: boolean) -> ())?,
    on_pointer: ((event: ThingPointerEvent) -> boolean?)?,
}

export type ThingPointerAreaProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    children: ThingChildren?,
    on_pointer: ((event: ThingPointerEvent) -> boolean?)?,
    on_scroll: ThingScrollHandler?,
}

export type ThingSliderProperties = {
    key: string?,
    autofocus: boolean?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    min: ThingReactive<number>?,
    max: ThingReactive<number>?,
    value: ThingReactive<number>?,
    step: ThingReactive<number>?,
    on_change: ((value: number) -> ())?,
    on_hover: ((inside: boolean) -> ())?,
}

export type ThingRangeProperties = {
    key: string?,
    autofocus: boolean?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    children: ThingChildren?,
    min: ThingReactive<number>?,
    max: ThingReactive<number>?,
    value: ThingReactive<number>?,
    step: ThingReactive<number>?,
    inset: ThingReactive<number>?,
    on_change: ((value: number) -> ())?,
    on_pressed: ((down: boolean) -> ())?,
    on_hover: ((inside: boolean) -> ())?,
}

export type ThingPopupProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    children: ThingChildren?,
    open_on_hover: ThingReactive<boolean>?,
    open_on_click: ThingReactive<boolean>?,
    open_delay: ThingReactive<number>?,
    close_delay: ThingReactive<number>?,
    anchor: ThingReactive<ThingPopupAnchor>?,
    alignment: ThingReactive<ThingPopupAlignment>?,
    offset: ThingReactive<ThingPopupOffset?>?,
    constraints: ThingReactive<ThingPopupConstraints?>?,
    idle_inhibit: ThingReactive<boolean>?,
    on_key: ((event: ThingKeyEvent) -> boolean)?,
    on_hover: ((inside: boolean) -> ())?,
    on_close: (() -> ())?,
}

declare extern type ThingAudioDevice with
    read name: string
    read description: string
end

declare extern type ThingAudioSource with
    read name: string
    read description: string
    read monitor: boolean
end

declare extern type ThingAudioStream with
    read id: string
    read application: string
    read app_id: string
    read name: string
    read icon: string
    read volume: number
    read muted: boolean
    read volume_writable: boolean
end

declare extern type ThingDesktopAction with
    read key: string
    read name: string
    read icon: string
    read command: ThingReadonlyArray<string>
end

declare extern type ThingDesktopApplication with
    read id: string
    read name: string
    read generic_name: string
    read comment: string
    read icon: string
    read keywords: ThingReadonlyArray<string>
    read categories: ThingReadonlyArray<string>
    read command: ThingReadonlyArray<string>
    read working_directory: string?
    read terminal: boolean
    read no_display: boolean
    read startup_wm_class: string
    read dbus_activatable: boolean
    read actions: ThingReadonlyArray<ThingDesktopAction>
end

declare extern type ThingMediaPlayer with
    read id: string
    read name: string
    read playing: boolean
    read title: string
    read artist: string
    read art_url: string
    read identity: string
    read desktop_entry: string
    read can_play_pause: boolean
    read can_next: boolean
    read can_previous: boolean
    read can_stop: boolean
    read can_seek: boolean
    read length_ms: number?
    read rate: number
end

declare extern type ThingPowerDevice with
    read id: string
    read kind: string
    read state: string
    read power_supply: boolean
    read present: boolean?
    read online: boolean?
    read percentage: number?
    read capacity: number?
    read time_to_empty: number?
    read time_to_full: number?
    read energy: number?
    read energy_empty: number?
    read energy_full: number?
    read energy_full_design: number?
    read energy_rate: number?
    read native_path: string?
    read vendor: string?
    read model: string?
    read serial: string?
    read icon_name: string?
    read warning_level: string
    read battery_level: string?
end

declare extern type ThingNotificationAction with
    read key: string
    read label: string
end

declare extern type ThingNotification with
    read key: string
    read id: number
    read revision: number
    read app_name: string
    read app_icon: string
    read summary: string
    read body: string
    read actions: ThingReadonlyArray<ThingNotificationAction>
    read urgency: "low" | "normal" | "critical"
    read urgency_code: number
    read raw_expire_timeout_ms: number
    read effective_timeout_ms: number?
    read created_at_ms: number
    read updated_at_ms: number
    read expires_at_ms: number?
    read category: string
    read desktop_entry: string
    read image_path: string
    read resident: boolean
    read transient: boolean
    read action_icons: boolean
end

declare extern type ThingSystemTrayItem with
    read id: string
    read protocol_item_id: string
    read category: string
    read title: string
    read status: string
    read icon: ThingImageSource?
    read icon_name: string
    read attention_icon_name: string
    read overlay_icon_name: string
    read tooltip_icon_name: string
    read tooltip_title: string
    read tooltip_text: string
    read item_is_menu: boolean
    read has_menu: boolean
    read menu: ThingSystemTrayMenu?
    read has_icon_pixmap: boolean
    read has_attention_icon_pixmap: boolean
    read has_overlay_icon_pixmap: boolean
    read has_tooltip_icon_pixmap: boolean
end

declare extern type ThingSystemTrayMenu with
    read id: string
    read loaded: boolean
    read loading: boolean
    read error: string
    read children: ThingReadonlyArray<ThingSystemTrayMenuItem>
end

declare extern type ThingSystemTrayMenuItem with
    read id: string
    read kind: string
    read label: string
    read access_key: string?
    read enabled: boolean
    read visible: boolean
    read icon_name: string
    read shortcut: ThingReadonlyArray<ThingReadonlyArray<string>>
    read toggle_type: string
    read toggle_state: number
    read disposition: string
    read has_submenu: boolean
    read loaded: boolean
    read loading: boolean
    read error: string
    read children: ThingReadonlyArray<ThingSystemTrayMenuItem>
end

declare extern type ThingNiriWorkspace with
    read id: string
    read index: number
    read name: string?
    read output: string?
    read active: boolean
    read focused: boolean
    read urgent: boolean
end

declare extern type ThingNiriWindow with
    read id: string
    read workspace_id: string?
    read title: string?
    read app_id: string?
    read focused: boolean
    read urgent: boolean
    read floating: boolean
    read column: number?
    read row: number?
    read width: number
    read height: number
end

declare extern type ThingAudioService with
    read available: ThingSignal<boolean>
    read volume: ThingSignal<number>
    read muted: ThingSignal<boolean>
    read device: ThingSignal<string>
    read devices: ThingSignal<ThingReadonlyArray<ThingAudioDevice>>
    read streams: ThingSignal<ThingReadonlyArray<ThingAudioStream>>
    read source_available: ThingSignal<boolean>
    read source_volume: ThingSignal<number>
    read source_muted: ThingSignal<boolean>
    read source_device: ThingSignal<string>
    read sources: ThingSignal<ThingReadonlyArray<ThingAudioDevice>>
    read all_sources: ThingSignal<ThingReadonlyArray<ThingAudioSource>>
    read error: ThingSignal<string>
    read set_volume: (volume: number) -> ()
    read set_muted: (muted: boolean) -> ()
    read set_default_sink: (name: string) -> ()
    read set_stream_volume: (id: string, volume: number) -> ()
    read set_stream_muted: (id: string, muted: boolean) -> ()
    read set_source_volume: (volume: number) -> ()
    read set_source_muted: (muted: boolean) -> ()
    read set_default_source: (name: string) -> ()
end

export type ThingAuthenticationState =
    "idle" | "authenticating" | "waiting" | "rejected" | "unavailable"

declare extern type ThingAuthenticationService with
    read available: ThingSignal<boolean>
    read state: ThingSignal<ThingAuthenticationState>
    read prompt: ThingSignal<string>
    read message: ThingSignal<string>
end

declare extern type ThingClipboardService with
    read available: ThingSignal<boolean>
    read text: ThingSignal<string?>
    read loading: ThingSignal<boolean>
    read error: ThingSignal<string>
    read set_text: (text: string) -> ()
    read clear: () -> ()
end

declare extern type ThingClockService with
    read time: ThingSignal<string>
    read date: ThingSignal<string>
end

declare extern type ThingIdleService with
    read inhibit_available: ThingSignal<boolean>
end

declare extern type ThingApplicationsService with
    read available: ThingSignal<boolean>
    read error: ThingSignal<string>
    read entries: ThingSignal<ThingReadonlyArray<ThingDesktopApplication>>
end

declare extern type ThingMediaService with
    read available: ThingSignal<boolean>
    read selected_id: ThingSignal<string>
    read playing: ThingSignal<boolean>
    read title: ThingSignal<string>
    read artist: ThingSignal<string>
    read art_url: ThingSignal<string>
    read identity: ThingSignal<string>
    read desktop_entry: ThingSignal<string>
    read can_play_pause: ThingSignal<boolean>
    read can_next: ThingSignal<boolean>
    read can_previous: ThingSignal<boolean>
    read can_stop: ThingSignal<boolean>
    read can_seek: ThingSignal<boolean>
    read length_ms: ThingSignal<number?>
    read rate: ThingSignal<number>
    read error: ThingSignal<string>
    read players: ThingSignal<ThingReadonlyArray<ThingMediaPlayer>>
    read play_pause: (player: string?) -> ()
    read next: (player: string?) -> ()
    read previous: (player: string?) -> ()
    read stop: (player: string?) -> ()
    read seek: (offset_ms: number, player: string?) -> ()
    read set_position: (position_ms: number, player: string?) -> ()
    read position: (player: string?) -> number?
end

declare extern type ThingPowerService with
    read available: ThingSignal<boolean>
    read on_battery: ThingSignal<boolean>
    read error: ThingSignal<string>
    read display: ThingSignal<ThingPowerDevice?>
    read devices: ThingSignal<ThingReadonlyArray<ThingPowerDevice>>
end

declare extern type ThingNotificationService with
    read owned: ThingSignal<boolean>
    read error: ThingSignal<string>
    read active: ThingSignal<ThingReadonlyArray<ThingNotification>>
    read dismiss: (key: string) -> ()
    read invoke: (key: string, action: string) -> ()
end

declare extern type ThingSystemTrayService with
    read available: ThingSignal<boolean>
    read watcher_owned: ThingSignal<boolean>
    read error: ThingSignal<string>
    read items: ThingSignal<ThingReadonlyArray<ThingSystemTrayItem>>
    read activate: (id: string) -> ()
    read secondary_activate: (id: string) -> ()
    read context_menu: (id: string) -> ()
    read open_menu: (id: string) -> ()
    read close_menu: (id: string) -> ()
    read activate_menu_item: (id: string) -> ()
end

declare extern type ThingNiriService with
    read available: ThingSignal<boolean>
    read error: ThingSignal<string>
    read workspaces: ThingSignal<ThingReadonlyArray<ThingNiriWorkspace>>
    read windows: ThingSignal<ThingReadonlyArray<ThingNiriWindow>>
    read focus_workspace: (id: string) -> ()
    read focus_window: (id: string) -> ()
    read close_window: (id: string) -> ()
end

declare extern type ThingOutputService with
    read available: ThingSignal<boolean>
    read name: ThingSignal<string>
    read width: ThingSignal<number>
    read height: ThingSignal<number>
    read scale: ThingSignal<number>
end

declare extern type ThingManagedProcess with
    read status: ThingSignal<ThingManagedProcessStatus>
    read running: ThingSignal<boolean>
    read stdout: ThingSignal<string>
    read stderr: ThingSignal<string>
    read stdout_truncated: ThingSignal<boolean>
    read stderr_truncated: ThingSignal<boolean>
    read exit_code: ThingSignal<number?>
    read exit_signal: ThingSignal<number?>
    read error: ThingSignal<string>
    read start: (arguments: { string }, working_directory: string?) -> ()
    read stop: () -> ()
end

declare extern type ThingScheduledProcess with
    read status: ThingSignal<ThingManagedProcessStatus>
    read running: ThingSignal<boolean>
    read stdout: ThingSignal<string>
    read stderr: ThingSignal<string>
    read stdout_truncated: ThingSignal<boolean>
    read stderr_truncated: ThingSignal<boolean>
    read exit_code: ThingSignal<number?>
    read exit_signal: ThingSignal<number?>
    read error: ThingSignal<string>
    read refresh: () -> ()
    read cancel: () -> ()
end

declare extern type ThingServices with
    read applications: ThingApplicationsService
    read audio: ThingAudioService
    read authentication: ThingAuthenticationService
    read clipboard: ThingClipboardService
    read clock: ThingClockService
    read idle: ThingIdleService
    read media: ThingMediaService
    read notifications: ThingNotificationService
    read niri: ThingNiriService
    read output: ThingOutputService
    read power: ThingPowerService
    read system_tray: ThingSystemTrayService
    read process: (
        ((key: string) -> ThingManagedProcess)
        & ((options: ThingScheduledProcessOptions) -> ThingScheduledProcess)
        & ((options: ThingManagedProcessOptions) -> ThingManagedProcess)
    )
    read launch: (arguments: { string }, working_directory: string?) -> ()
end

declare ui: {
    panel: (properties: ThingPanelProperties) -> ThingWidget,
    lock: (properties: ThingLockProperties) -> ThingWidget,
    popup: (properties: ThingPopupProperties) -> ThingWidget,
    row: (properties: ThingContainerProperties) -> ThingWidget,
    centerbox: (properties: ThingContainerProperties) -> ThingWidget,
    column: (properties: ThingContainerProperties) -> ThingWidget,
    stack: (properties: ThingContainerProperties) -> ThingWidget,
    scroll: (properties: ThingContainerProperties) -> ThingWidget,
    text: (properties: ThingTextProperties) -> ThingWidget,
    text_input: (properties: ThingTextInputProperties) -> ThingWidget,
    secret_input: (properties: ThingSecretInputProperties) -> ThingWidget,
    image: (properties: ThingImageProperties) -> ThingWidget,
    path: (properties: ThingPathProperties) -> ThingWidget,
    button: (properties: ThingButtonProperties) -> ThingWidget,
    pointer_area: (properties: ThingPointerAreaProperties) -> ThingWidget,
    slider: (properties: ThingSliderProperties) -> ThingWidget,
    range: (properties: ThingRangeProperties) -> ThingWidget,
    focus: (widget: ThingWidget) -> (),
    reveal: (widget: ThingWidget) -> (),
    state: <T>(initial: T & ThingStateValue) -> ThingState<T>,
    timer: (options: ThingTimerOptions) -> ThingTimer,
    transition: (
        ((value: () -> number, options: ThingTransitionOptions) -> ThingNumberTransition)
        & ((value: () -> string, options: ThingTransitionOptions) -> ThingColorTransition)
    ),
    spring: (value: () -> number, options: ThingSpringOptions) -> ThingNumberSpring,
    asset: (relative_path: string) -> string,
}

declare services: ThingServices
