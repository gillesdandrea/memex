.pragma library

function object(value) {
    if (typeof value === "object" && value !== null && !Array.isArray(value)) return value;
    if (typeof value !== "string" || !/^\s*\{/.test(value)) return {};
    try { var parsed = JSON.parse(value); return parsed && !Array.isArray(parsed) ? parsed : {}; }
    catch (_) { return {}; }
}
function string(value) { return value === undefined || value === null ? "" : (typeof value === "string" ? value : JSON.stringify(value, null, 2)); }
function compact(value) { var s = string(value).replace(/\s+/g, " ").trim(); return s.length > 88 ? s.slice(0,87) + "…" : s; }
function activity(r) { return ["tool_use", "tool_result", "tool", "reasoning"].indexOf(r.role) >= 0; }
function instruction(r) { return !!r.context_label || ["system", "developer"].indexOf(r.role) >= 0; }
function body(entry) {
    var r = entry.record, values = [];
    if (!activity(r) && !instruction(r)) return string(r.text);
    [r.tool_input, r.tool_output, r.text].forEach(function(value) {
        var text = string(value); if (text.trim() && values.indexOf(text) < 0) values.push(text);
    });
    return values.join("\n\n");
}
function failure(value) {
    var o = object(value);
    return o.isError === true || o.is_error === true || (typeof o.exit_code === "number" && o.exit_code !== 0);
}
function attention(records) {
    return records.some(function(e) {
        var r = e.record, result = ["tool_result", "tool"].indexOf(r.role) >= 0;
        var output = object(r.tool_output || (result ? r.text : ""));
        return (r.role === "tool_result" && r.tool_result_is_error === true) || failure(r.tool_output)
            || (result && failure(r.text)) || ["cancelled", "canceled", "interrupted", "incomplete"].indexOf(string(output.status).toLowerCase()) >= 0;
    });
}
function title(records) {
    var entry = records.filter(function(e) { return e.record.role === "tool_use"; })[0] || records[0];
    var r = entry.record, name = string(r.tool_name).split("__").pop().split(".").pop().toLowerCase(), input = object(r.tool_input);
    function detail(keys) { for (var i=0; i<keys.length; i++) if (typeof input[keys[i]] === "string" && input[keys[i]].trim()) return " " + compact(input[keys[i]]); return ""; }
    var label = r.context_label;
    if (!label && instruction(r)) label = r.role + " instructions";
    if (!label && r.role === "reasoning") label = "Reasoning";
    if (!label && r.role === "assistant") label = compact(r.text);
    if (!label) {
        if (["read", "read_file", "readfile"].indexOf(name)>=0) label = "Read" + detail(["file_path", "path", "filename"]);
        else if (["grep", "search", "search_code", "search_files", "search_query", "ripgrep"].indexOf(name)>=0) label = "Search" + detail(["pattern", "query", "q", "search_term"]);
        else if (["glob", "find_files", "list_files", "list_directory", "ls"].indexOf(name)>=0) label = "List files" + detail(["pattern", "glob", "path", "directory"]);
        else if (["bash", "shell", "shell_command", "exec_command", "run_command", "terminal"].indexOf(name)>=0 || (name === "exec" && (input.command || input.cmd))) label = "Run" + detail(["command", "cmd"]);
        else if (r.tool_name === "functions.exec") label = "Run tool script";
        else if (["wait", "write_stdin"].indexOf(name)>=0) label = "Wait for command";
        else if (["write", "write_file", "writefile", "create_file"].indexOf(name)>=0) label = "Write" + detail(["file_path", "path", "filename"]);
        else if (["edit", "edit_file", "editfile", "multiedit", "multi_edit", "replace_in_file"].indexOf(name)>=0) label = "Edit" + detail(["file_path", "path", "filename"]);
        else if (name === "apply_patch") label = "Apply patch";
        else if (["view_image", "open_image"].indexOf(name)>=0) label = "View image" + detail(["path", "image_path"]);
        else if (["web_fetch", "webfetch", "fetch_url"].indexOf(name)>=0) label = "Fetch" + detail(["url"]);
        else label = r.tool_name || (r.role === "tool_result" ? "Tool result" : "Tool activity");
    }
    return (attention(records) ? "Needs attention · " : "") + label;
}
function pair(records) {
    var result = [], tools = [];
    function flush() {
        var partners = {}, used = {}, calls = [], outputs = [], linked = Object.create(null);
        tools.forEach(function(e,i) {
            if (e.record.role === "tool_use") { calls.push(i); var id = e.record.event_id; if (id && string(id).trim()) { if (!linked[id]) linked[id]=[]; linked[id].push(i); } }
            else outputs.push(i);
        });
        outputs.forEach(function(i) {
            var matches = linked[tools[i].record.parent_tool_use_id];
            if (matches && matches.length === 1 && partners[matches[0]] === undefined) { partners[matches[0]]=i; used[i]=true; }
        });
        outputs.forEach(function(i) {
            if (used[i] || i===0) return;
            var c=i-1, a=tools[c].record, b=tools[i].record;
            if (a.role !== "tool_use" || partners[c] !== undefined) return;
            if (a.event_id && b.parent_tool_use_id && a.event_id !== b.parent_tool_use_id) return;
            if (a.tool_name && b.tool_name && a.tool_name !== b.tool_name) return;
            if (calls.some(function(n) { return n<c && (partners[n] === undefined || partners[n]>i); })) return;
            partners[c]=i; used[i]=true;
        });
        tools.forEach(function(e,i) { if (!used[i]) result.push(partners[i] !== undefined ? [e,tools[partners[i]]] : [e]); });
        tools=[];
    }
    records.forEach(function(e) { if (["tool_use","tool_result"].indexOf(e.record.role)>=0) tools.push(e); else { flush(); result.push([e]); } });
    flush(); return result;
}
function assistantText(text) {
    if (text.indexOf(":codex-annotation")<0 && text.indexOf("<oai-mem-citation>")<0) return text;
    var protectedRanges=[], offset=0, fence=null, ticks=0;
    text.split("\n").forEach(function(line) {
        var trimmed=line.replace(/^\s+/, ""), marker=/^(`{3,}|~{3,})/.exec(trimmed);
        if (fence) { protectedRanges.push([offset,offset+line.length]); if (marker && marker[1][0]===fence[0] && marker[1].length>=fence.length && trimmed.slice(marker[1].length).trim()==="") fence=null; }
        else if (marker) { fence=marker[1]; protectedRanges.push([offset,offset+line.length]); }
        else if (/^\s*>/.test(line) || /^(    |\t)/.test(line)) protectedRanges.push([offset,offset+line.length]);
        else {
            var re=/`+/g, match, start=ticks ? 0 : -1;
            while ((match=re.exec(line))) {
                if (ticks===match[0].length) { protectedRanges.push([offset+start,offset+re.lastIndex]); ticks=0; start=-1; }
                else if (!ticks) { ticks=match[0].length; start=match.index; }
            }
            if (start>=0) protectedRanges.push([offset+start,offset+line.length]);
        }
        offset+=line.length+1;
    });
    function protectedAt(start,end) { return protectedRanges.some(function(r) { return start<r[1] && end>r[0]; }); }
    var removals=[], citation=/(?:^|\n)[ \t]*<oai-mem-citation>\s*<citation_entries>[^<>]*<\/citation_entries>\s*<rollout_ids>[^<>]*<\/rollout_ids>\s*<\/oai-mem-citation>\s*$/.exec(text);
    if (citation && !protectedAt(citation.index,citation.index+citation[0].length)) removals.push([citation.index,citation.index+citation[0].length]);
    var annotations=/::?codex-annotation\{index="[0-9]+"\}/g, a;
    while ((a=annotations.exec(text))) {
        var before=text.charAt(a.index-1), after=text.charAt(annotations.lastIndex);
        if (before!==":" && after!=="}" && !/["'“‘]/.test(before) && !/["'”’]/.test(after) && !protectedAt(a.index,annotations.lastIndex)
            && !removals.some(function(r) { return a.index<r[1] && annotations.lastIndex>r[0]; })) removals.push([a.index,annotations.lastIndex]);
    }
    removals.sort(function(a,b) { return b[0]-a[0]; });
    removals.forEach(function(r) { text=text.slice(0,r[0])+text.slice(r[1]); });
    return removals.length ? text.trim() : text;
}
function attachments(r) {
    var values=r.source_content;
    if (typeof values === "string") { try { values=JSON.parse(values); } catch (_) { return []; } }
    if (!Array.isArray(values)) return [];
    var results=[];
    values.forEach(function(v) {
        var source=v.source || {}, type=v.type, label=v.title || v.filename || "Attachment", url="", image=false;
        if (["input_image","image","image_url","local_image","localImage"].indexOf(type)>=0) {
            image=true; if (label==="Attachment") label="Image";
            var data=source.data || v.data;
            if (typeof data === "string" && data.length<=28000000 && /^[A-Za-z0-9+/]*={0,2}$/.test(data)) url="data:"+(source.media_type || v.mimeType || "image/png")+";base64,"+data;
            else url=(typeof v.image_url === "string" ? v.image_url : (v.image_url || {}).url) || v.url || source.url || v.path || "";
        } else if (["document","input_file","file","attachment"].indexOf(type)>=0) {
            url=v.path || v.file_url || source.url || "";
            if (!url && source.type==="text" && typeof source.data === "string") { results.push({label:label,code:source.data}); return; }
            if (!url) { results.push({label:label,notice:v.file_id ? v.file_id+" · Attachment is stored with the provider." : "Embedded document; contents retained in the raw transcript."}); return; }
        } else return;
        if (/^(https?:\/\/|file:\/\/|\/|data:image\/)/.test(url)) results.push({label:label,url:url,image:image});
    });
    return results;
}
function project(records) {
    var result=[], tags={recommended_plugins:"Available plugins",environment_context:"Environment context","permissions instructions":"Permissions",skills_instructions:"Skills","app-context":"Application context",collaboration_mode:"Collaboration mode",context_window:"Context window"};
    records.forEach(function(source) {
        var r=Object.assign({},source.record), entry={record_id:source.record_id,source_id:source.source_id || source.record_id,record:r,raw:source.raw || JSON.stringify(source,null,2)};
        r.text=string(r.text);
        var images=attachments(r).filter(function(a) { return a.image; }).length;
        if (images) r.text=r.text.split("\n").filter(function(line) { if (images && line.trim()==="<<ImageDisplayed>>") { images--; return false; } return true; }).join("\n");
        if (r.role==="assistant") r.text=assistantText(r.text);
        if (r.role!=="user" || r.context_label) { result.push(entry); return; }
        function piece(id,text,label) { var message=Object.assign({},r); message.text=text; if (label) message.context_label=label; return {record_id:id,source_id:entry.source_id,record:message,raw:entry.raw}; }
        var reply=/^\s*<send_user_message_question_reply>([\s\S]*)<\/send_user_message_question_reply>\s*$/.exec(r.text);
        if (reply) {
            var replies; try { replies=JSON.parse(reply[1]); } catch (_) {}
            if (Array.isArray(replies) && replies.length && replies.every(function(a) { return typeof a.question==="string" && typeof a.answer==="string" && typeof a.questionItemId==="string"; })) {
                replies.forEach(function(a,i) { result.push(piece(entry.record_id+":question:"+i,a.question,"Question")); result.push(piece(i===0 ? entry.record_id : entry.record_id+":answer:"+i,a.answer)); }); return;
            }
        }
        var remaining=r.text, pieces=[];
        while (remaining) {
            var trimmed=remaining.replace(/^\s+/,""), end=-1,label="";
            Object.keys(tags).some(function(tag) { if (trimmed.indexOf("<"+tag+">")!==0) return false; var close=remaining.indexOf("</"+tag+">"); if (close<0) return false; end=close+tag.length+3; label=tags[tag]; return true; });
            if (end<0 && /^# AGENTS\.md instructions(?: for [^\n]*|)\n\s*<INSTRUCTIONS>/.test(trimmed)) { var close=remaining.indexOf("</INSTRUCTIONS>"); if (close>=0) { end=close+15; label="Project instructions"; } }
            if (end<0) break;
            pieces.push({text:remaining.slice(0,end),label:label}); remaining=remaining.slice(end);
        }
        if (!pieces.length) { result.push(entry); return; }
        var hasRequest=!!remaining.trim();
        if (!hasRequest) pieces[pieces.length-1].text+=remaining;
        pieces.forEach(function(p,i) { result.push(piece(!hasRequest && pieces.length===1 ? entry.record_id : entry.record_id+":context:"+i,p.text,p.label)); });
        if (hasRequest) result.push(piece(entry.record_id,remaining));
    });
    return result;
}
function group(source,rawMode,findMode) {
    // Find's occurrence belongs to the complete source body, before context
    // splitting, placeholder replacement, or transport-marker projection.
    var records=rawMode || findMode ? source.map(function(e) { return {record_id:e.record_id,source_id:e.record_id,record:e.record,raw:JSON.stringify(e,null,2)}; }) : project(source);
    if (rawMode && !findMode) return records.map(function(e) { return item([e],false,false); });
    var turns=Object.create(null), output=[], pending=[], kind="", turn="";
    records.forEach(function(e) { var r=e.record; if (!r.source_turn_id) return; var t=turns[r.source_turn_id] || {final:false,complete:false,aborted:false}; t.final=t.final || (r.role==="assistant" && r.assistant_phase==="final_answer"); t.complete=t.complete || r.lifecycle_event==="task_complete"; t.aborted=t.aborted || r.lifecycle_event==="turn_aborted"; turns[r.source_turn_id]=t; });
    function flush() {
        if (!pending.length) return;
        var activities=pair(pending), routine=[];
        function flushRoutine() { if (routine.length) { output.push(item([].concat.apply([],routine),kind==="work",kind!=="message",routine)); routine=[]; } }
        activities.forEach(function(a) { if (attention(a)) { flushRoutine(); output.push(item(a,false,true,[a])); } else routine.push(a); });
        flushRoutine(); pending=[];
    }
    records.forEach(function(e) {
        var r=e.record, t=turns[r.source_turn_id], k="message";
        if (r.role==="lifecycle" && ["task_started","task_complete"].indexOf(r.lifecycle_event)>=0) { flush(); return; }
        if (!instruction(r) && t && t.final && t.complete && !t.aborted && (activity(r) || (r.role==="assistant" && r.assistant_phase==="commentary"))) k="work";
        else if (instruction(r)) k="context"; else if (activity(r)) k="activity";
        if (k!==kind || k==="message" || (k==="work" && turn!==r.source_turn_id)) flush();
        kind=k; turn=r.source_turn_id; pending.push(e);
        if (k==="message") flush();
    });
    flush(); return output;
}
function item(records,completed,collapsible,activities) {
    return {id:records[0].record_id,records:records,activities:activities || pair(records),completed:completed,collapsible:collapsible,attention:attention(records),title:completed ? "Completed work" : (instruction(records[0].record) ? "Session context" : title(records))};
}
function rawBody(item) { return item.records.map(function(e) { return e.raw || JSON.stringify(e,null,2); }).join("\n\n"); }
function itemBody(item) { return item.records.map(body).join("\n\n"); }
function indexForRecord(items,id) {
    // Prefer the actual request over its synthetic context pieces.
    for (var i=0;i<items.length;i++) if (items[i].records.some(function(e) { return e.record_id===id; })) return i;
    for (var j=0;j<items.length;j++) if (items[j].records.some(function(e) { return e.source_id===id; })) return j;
    return -1;
}

function markdownBlocks(text) {
    var lines=text.split("\n"), blocks=[], prose=[], code=[], fence="", language="";
    function flushProse() { if (prose.length) { blocks.push({code:false,text:prose.join("\n"),language:""}); prose=[]; } }
    lines.forEach(function(line) {
        var marker=/^ {0,3}(`{3,}|~{3,})(.*)$/.exec(line);
        if (!fence && marker) { flushProse(); fence=marker[1]; language=marker[2].trim().split(/\s+/)[0]; code=[]; }
        else if (fence && marker && marker[1][0]===fence[0] && marker[1].length>=fence.length && !marker[2].trim()) { blocks.push({code:true,text:code.join("\n"),language:language}); fence=""; code=[]; }
        else if (fence) code.push(line); else prose.push(line);
    });
    if (fence) blocks.push({code:true,text:code.join("\n"),language:language});
    flushProse(); return blocks;
}
function escapeHtml(text) { return string(text).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;"); }
function highlightCode(text,language) {
    var lang=string(language).toLowerCase();
    if (["diff","patch"].indexOf(lang)>=0) return text.split("\n").map(function(line) {
        var color=/^\+/.test(line) ? "#43845b" : /^-/.test(line) ? "#c26060" : /^(@@|\*\*\*)/.test(line) ? "#688bc3" : "";
        return color ? '<span style="color:'+color+'">'+escapeHtml(line)+"</span>" : escapeHtml(line);
    }).join("\n");
    if (["javascript","js","typescript","ts","rust","rs","python","py","json","bash","sh","shell","swift","sql","qml","cpp","c","c++"].indexOf(lang)<0) return escapeHtml(text);
    var re=/("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\/\/[^\n]*|#[^\n]*|\b(?:true|false|null|undefined|None|True|False|let|var|const|function|fn|pub|use|mod|struct|enum|impl|return|if|else|for|while|match|async|await|import|from|def|class|select|from|where|as|join|on|int|void|property|signal)\b|\b\d+(?:\.\d+)?\b)/gi;
    var result="", cursor=0, match;
    while ((match=re.exec(text))) {
        result+=escapeHtml(text.slice(cursor,match.index));
        var token=match[0], color=/^["']/.test(token) ? "#43845b" : /^(\/\/|#)/.test(token) ? "#888888" : /^\d/.test(token) ? "#b98539" : "#9380c7";
        result+='<span style="color:'+color+'">'+escapeHtml(token)+"</span>"; cursor=re.lastIndex;
    }
    return result+escapeHtml(text.slice(cursor));
}
function toolSections(record) {
    var sections=[], name=string(record.tool_name).split("__").pop().split(".").pop().toLowerCase();
    var values=[];
    [record.tool_input,record.tool_output,record.text].forEach(function(value) { value=string(value); if (value.trim() && values.indexOf(value)<0) values.push(value); });
    values.forEach(function(value) {
        var parsed=object(value), keys=Object.keys(parsed);
        if (!keys.length) sections.push({label:"",text:value,language:name==="apply_patch" || /^\*\*\* Begin Patch|^diff --git|^@@/m.test(value) ? "diff" : record.tool_name==="functions.exec" && record.role==="tool_use" ? "javascript" : "text"});
        else keys.forEach(function(key) {
            var text=string(parsed[key]), language=typeof parsed[key]==="object" ? "json" : ["patch","diff"].indexOf(key)>=0 ? "diff" : ["cmd","command"].indexOf(key)>=0 ? "sh" : key==="code" ? "javascript" : "text";
            sections.push({label:key,text:text,language:language,path:["path","file_path","filename"].indexOf(key)>=0 && text.charAt(0)==="/" ? text : ""});
        });
    });
    return sections;
}
function inlineImageUrl(url,session) {
    url=string(url);
    if (url.indexOf("data:image/")===0) return url;
    // A remote machine's absolute paths must never resolve on this host.
    if (session && session.machine && session.machine!=="local") return "";
    if (/^\/(?!\/)/.test(url)) return "file://"+url;
    if (/^file:\/\//.test(url) && !/^file:\/\/[^/]/.test(url)) return url;
    if (url && !/^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(url) && session && /^\/(?!\/)/.test(string(session.cwd))) {
        var pieces=(session.cwd+"/"+url).split("/"), resolved=[];
        pieces.forEach(function(piece) { if (!piece || piece===".") return; if (piece==="..") resolved.pop(); else resolved.push(piece); });
        return "file:///"+resolved.join("/");
    }
    return "";
}

function safeMarkdown(text) {
    var images=[], references=Object.create(null), reference=/^ {0,3}\[([^\]]+)\]:\s*(?:<([^>]+)>|(\S+))/gm, match;
    while ((match=reference.exec(text))) references[match[1].toLowerCase()]=match[2] || match[3];
    function image(label,url) {
        images.push({label:label || "Image",url:url,image:true});
        return "Image: "+(label || "attachment");
    }
    function prose(value) {
        value=value.replace(/!\[([^\]]*)\]\(\s*(?:<([^>]+)>|((?:[^\s()\\]|\\.|\([^()]*\))+))(?:\s+["'][^\n]*?["'])?\s*\)/g,function(_,label,angle,url) { return image(label,angle || url); });
        value=value.replace(/!\[([^\]]*)\]\[([^\]]*)\]/g,function(original,label,id) { var url=references[(id || label).toLowerCase()]; return url ? image(label,url) : original; });
        value=value.replace(/!\[([^\]]+)\]/g,function(original,label) { var url=references[label.toLowerCase()]; return url ? image(label,url) : original; });
        value=value.replace(/<img\b[^>]*>/gi,function(tag) {
            var src=/\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/i.exec(tag), alt=/\balt\s*=\s*(?:"([^"]*)"|'([^']*)')/i.exec(tag);
            return src ? image(alt ? alt[1] || alt[2] : "Image",src[1] || src[2] || src[3]) : escapeHtml(tag);
        });
        // Even an unsupported image dialect must not be handed to Qt's resource
        // loader. The original remains available in Raw and source-based Find.
        return value.replace(/!\[/g,"\\![");
    }
    var result="", cursor=0, ticks=/`+/g;
    while ((match=ticks.exec(text))) {
        var start=match.index, run=match[0], end=text.indexOf(run,ticks.lastIndex);
        if (end<0) continue;
        result+=prose(text.slice(cursor,start))+text.slice(start,end+run.length);
        cursor=end+run.length;
        ticks.lastIndex=cursor;
    }
    result+=prose(text.slice(cursor));
    return {text:result,images:images};
}
