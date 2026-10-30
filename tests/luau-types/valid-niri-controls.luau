--!strict

local niri = services.niri
return ui.panel { children = {
    ui.button {
        on_click = function() niri.switch_layout("next") end,
        on_secondary_click = function() niri.switch_layout(1) end,
        on_middle_click = function() niri.toggle_overview(); niri.set_overview(false) end,
        children = { ui.text { text = function()
            local layouts = niri.keyboard_layouts:get()
            if layouts and layouts.current_index then
                local name: string = layouts.names[layouts.current_index]
                return name
            end
            return "no layout"
        end } },
    },
    ui.text { text = function()
        local opened: boolean? = niri.overview_open:get()
        return if opened then "overview" else "desktop"
    end },
} }
