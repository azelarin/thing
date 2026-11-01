--!strict

local open = ui.state(false)
local tab = ui.state("apps")

ui.command("toggle-launcher", function(arguments)
    if arguments and type(arguments.tab) == "string" then
        tab:set(arguments.tab)
    end
    open:set(not open:get())
    return { open = open:get(), tab = tab:get() }
end)

ui.command("launcher.tabs", function()
    return { "apps", "files", "windows" }
end)

ui.command("close", function()
    open:set(false)
    return nil
end)

ui.command("selection", function(arguments)
    return { selected = if arguments == ui.null then ui.null else "apps" }
end)

return ui.panel {
    children = {
        ui.text { text = function() return if open:get() then tab:get() else "" end },
    },
}
