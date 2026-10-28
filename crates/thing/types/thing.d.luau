export type ThingStateValue = nil | boolean | number | string

export type ThingState<T> = {
    get: (self: ThingState<T>) -> T,
    set: (self: ThingState<T>, value: T) -> (),
}

export type ThingSignal<T> = {
    get: (self: ThingSignal<T>) -> T,
}

export type ThingReadonlyArray<T> = { T }

export type ThingReactive<T> = T | (() -> T)

declare extern type ThingWidget with end
declare extern type ThingTransition with end

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
export type ThingPopupAnchor = "auto" | ThingEdge
export type ThingPopupAlignment = "start" | "center" | "end"
export type ThingEasing = "linear" | "in_cubic" | "out_cubic" | "in_out_cubic"

export type ThingTransitionOptions = {
    duration: number,
    easing: ThingEasing?,
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
    | { rectangles: { any } }

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

type ThingAnimatedNumber = number | (() -> number) | ThingTransition
type ThingAnimatedOptionalNumber = number | (() -> number?) | ThingTransition

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
    border_color: ThingReactive<string>?,
    hover_overlay: ThingReactive<string>?,
    pressed_overlay: ThingReactive<string>?,
    disabled_opacity: ThingReactive<number>?,
    focus_ring_color: ThingReactive<string>?,
    focus_ring_width: ThingReactive<number>?,
    font_size: ThingReactive<number>?,
    font_weight: ThingReactive<number>?,
    font_family: ThingReactive<string>?,
    background: ThingReactive<string>?,
    foreground: ThingReactive<string>?,
    opacity: ThingAnimatedNumber?,
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
    border_color: ThingReactive<string>?,
    hover_overlay: ThingReactive<string>?,
    pressed_overlay: ThingReactive<string>?,
    disabled_opacity: ThingReactive<number>?,
    focus_ring_color: ThingReactive<string>?,
    focus_ring_width: ThingReactive<number>?,
    font_size: ThingReactive<number>?,
    font_weight: ThingReactive<number>?,
    font_family: ThingReactive<string>?,
    text_align: ThingReactive<ThingTextAlign>?,
    tabular_digits: ThingReactive<boolean>?,
    background: ThingReactive<string>?,
    foreground: ThingReactive<string>?,
    opacity: ThingAnimatedNumber?,
}

export type ThingChildren = { ThingWidget } | (() -> { ThingWidget })

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

export type ThingImageProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    source: ThingReactive<string?>?,
    icon: ThingReactive<string?>?,
    app_id: ThingReactive<string?>?,
    icon_theme: ThingReactive<string>?,
    fit: ThingReactive<ThingImageFit>?,
}

export type ThingButtonProperties = {
    key: string?,
    visible: ThingReactive<boolean>?,
    enabled: ThingReactive<boolean>?,
    style: ThingStyle?,
    children: ThingChildren?,
    on_click: (() -> ())?,
    on_middle_click: (() -> ())?,
    on_secondary_click: (() -> ())?,
    on_scroll: ((x: number, y: number) -> ())?,
    on_hover: ((inside: boolean) -> ())?,
}

export type ThingSliderProperties = {
    key: string?,
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
    read icon_name: string
    read attention_icon_name: string
    read overlay_icon_name: string
    read tooltip_icon_name: string
    read tooltip_title: string
    read tooltip_text: string
    read item_is_menu: boolean
    read has_menu: boolean
    read has_icon_pixmap: boolean
    read has_attention_icon_pixmap: boolean
    read has_overlay_icon_pixmap: boolean
    read has_tooltip_icon_pixmap: boolean
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

declare extern type ThingClockService with
    read time: ThingSignal<string>
    read date: ThingSignal<string>
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
    read error: ThingSignal<string>
    read players: ThingSignal<ThingReadonlyArray<ThingMediaPlayer>>
    read play_pause: (player: string?) -> ()
    read next: (player: string?) -> ()
    read previous: (player: string?) -> ()
    read stop: (player: string?) -> ()
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

declare extern type ThingServices with
    read audio: ThingAudioService
    read clock: ThingClockService
    read media: ThingMediaService
    read notifications: ThingNotificationService
    read niri: ThingNiriService
    read output: ThingOutputService
    read power: ThingPowerService
    read system_tray: ThingSystemTrayService
    read launch: (arguments: { string }, working_directory: string?) -> ()
end

declare ui: {
    panel: (properties: ThingPanelProperties) -> ThingWidget,
    popup: (properties: ThingPopupProperties) -> ThingWidget,
    row: (properties: ThingContainerProperties) -> ThingWidget,
    centerbox: (properties: ThingContainerProperties) -> ThingWidget,
    column: (properties: ThingContainerProperties) -> ThingWidget,
    stack: (properties: ThingContainerProperties) -> ThingWidget,
    scroll: (properties: ThingContainerProperties) -> ThingWidget,
    text: (properties: ThingTextProperties) -> ThingWidget,
    image: (properties: ThingImageProperties) -> ThingWidget,
    button: (properties: ThingButtonProperties) -> ThingWidget,
    slider: (properties: ThingSliderProperties) -> ThingWidget,
    range: (properties: ThingRangeProperties) -> ThingWidget,
    state: <T>(initial: T & ThingStateValue) -> ThingState<T>,
    transition: (value: () -> number, options: ThingTransitionOptions) -> ThingTransition,
    asset: (relative_path: string) -> string,
}

declare services: ThingServices
