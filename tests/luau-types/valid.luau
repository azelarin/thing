--!strict

local query = ui.state("")
local clipboard = services.clipboard
local width = ui.transition(function()
    return 280
end, { duration = 120 })
local accent = ui.transition(function()
    return "#89b4fa"
end, { duration = 120, easing = "out_cubic" })
local gap = ui.spring(function()
    return 8
end, { stiffness = 170, damping = 0.8, epsilon = 0.1 })

local input = ui.text_input {
    autofocus = true,
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
        gap = gap,
        background = accent,
        placeholder_color = "#ffffff80",
    },
}

local clipboard_button = ui.button {
    enabled = function()
        return clipboard.available:get() and not clipboard.loading:get()
    end,
    on_click = function()
        ui.focus(input)
        ui.reveal(input)
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

local pointer_area = ui.pointer_area {
    on_pointer = function(event)
        local local_position = event.x + event.y
        local surface_position = event.surface_x + event.surface_y
        local modified = event.shift or event.ctrl or event.alt or event.logo
        if event.phase == "cancel" or event.button == "secondary" then
            return modified and local_position ~= surface_position
        end
        return nil
    end,
    children = { ui.text { text = "drag region" } },
}

local pointer_button = ui.button {
    on_pointer = function(event)
        return event.phase == "press"
    end,
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
            local menu = item.menu
            if menu ~= nil then
                local label = menu.error
                for _, entry in menu.children do
                    label = entry.label
                    break
                end
                table.insert(children, ui.text { text = label })
            end
        end
        return children
    end,
}

local tray_menu_button = ui.button {
    on_click = function()
        local item = services.system_tray.items:get()[1]
        local menu = item.menu
        if menu == nil then return end

        services.system_tray.open_menu(menu.id)
        for _, entry in menu.children do
            if entry.has_submenu then
                services.system_tray.open_menu(entry.id)
                services.system_tray.close_menu(entry.id)
            elseif entry.visible and entry.enabled and entry.kind ~= "separator" then
                services.system_tray.activate_menu_item(entry.id)
            end
            break
        end
        services.system_tray.close_menu(menu.id)
    end,
    children = { ui.text { text = "menu" } },
}

local query_process = services.process("query")
local recorder_process = services.process {
    key = "recorder",
    shutdown_signal = "interrupt",
}
local process_button = ui.button {
    on_click = function()
        if query_process.running:get() then
            query_process.stop()
        else
            query_process.start({ "query-command", "--compact" }, "/tmp")
        end
        recorder_process.stop()
    end,
    children = {
        ui.text {
            text = function()
                local status: ThingManagedProcessStatus = query_process.status:get()
                local code: number? = query_process.exit_code:get()
                return `{status}:{code or "none"}:{query_process.stdout:get()}`
            end,
        },
    },
}

return ui.panel {
    on_key = function(event)
        if event.key == "Escape" or (event.ctrl and event.text == "c") then
            return true
        end
        return event.repeated and event.shift and not event.alt and not event.logo
    end,
    input_region = {
        rectangles = {
            { x = 0, y = 0, width = 320, height = 36 },
            { x = 420, width = 80 },
        },
    },
    children = {
        input,
        clipboard_button,
        pointer_area,
        pointer_button,
        tray,
        tray_menu_button,
        process_button,
    },
}
