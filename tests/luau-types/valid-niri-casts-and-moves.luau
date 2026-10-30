--!strict

local niri = services.niri
return ui.panel { children = {
    ui.button {
        on_click = function()
            niri.move_window_to_workspace("1", "2")
            niri.move_window_to_workspace("1", "2", false)
            niri.move_window_to_output("1", "DP-2")
            niri.move_workspace_to_output("2", "DP-1")
        end,
        children = { ui.text { text = function()
            local casts = niri.casts:get()
            if not casts then return "unknown" end
            for _, cast in casts do
                local stream_id: string = cast.stream_id
                local session_id: string = cast.session_id
                local active: boolean = cast.active
                local dynamic: boolean = cast.dynamic_target
                local pid: number? = cast.pid
                local node: number? = cast.pipewire_node_id
                local output: string? = cast.target.output
                local window: string? = cast.target.window_id
                local protocol: string? = cast.protocol_kind or cast.target.protocol_kind
                if active then return stream_id .. ":" .. cast.target.kind end
            end
            return "no active streams"
        end } },
    },
} }
