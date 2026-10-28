--!strict

local query = ui.state("")
local clipboard = services.clipboard
local width = ui.transition(function()
    return 280
end, { duration = 120 })
local accent = ui.transition(function()
    return "#89b4fa"
end, { duration = 120, easing = "out_cubic" })

local input = ui.text_input {
    value = function()
        return query:get()
    end,
    placeholder = "search applications",
    on_change = function(value)
        query:set(value)
    end,
    on_submit = function(value)
        clipboard.set_text(value)
    end,
    style = {
        width = width,
        background = accent,
        placeholder_color = "#ffffff80",
    },
}

local clipboard_button = ui.button {
    enabled = function()
        return clipboard.available:get() and not clipboard.loading:get()
    end,
    on_click = function()
        if clipboard.text:get() == nil then
            clipboard.set_text(query:get())
        else
            clipboard.clear()
        end
    end,
    children = {
        ui.text { text = function() return clipboard.error:get() end },
    },
}

local tray = ui.row {
    children = function()
        local children = {}
        for _, item in services.system_tray.items:get() do
            local source: ThingImageSource? = item.icon
            table.insert(children, ui.image { source = item.icon })
            table.insert(children, ui.image {
                source = function()
                    return source
                end,
            })
        end
        return children
    end,
}

return ui.panel { children = { input, clipboard_button, tray } }
