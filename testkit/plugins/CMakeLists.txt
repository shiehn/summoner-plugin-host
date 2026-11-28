# The lab's test plugins (this repo's). Built by tools/build-plugins.sh into target.noindex/plugins (cmake + ninja,
# nice 15): the DSP cores and their tests, and the VST3 and AU v2 wrappers over the build-time SDK fetch
# (VENDORED.md); no SDK's own CMake is run.
cmake_minimum_required(VERSION 3.22)
project(summoner_lab_plugins LANGUAGES C CXX OBJCXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Determinism for the null test: no FMA contraction, no fast-math; warnings are errors.
add_compile_options(-ffp-contract=off -fno-fast-math -Wall -Wextra -Wpedantic -Werror)
# -Wno-perf-constraint-implies-noexcept (below) only where the compiler knows the warning: an older clang rejects an
# unknown -Wno- flag under -Werror, so the build must not depend on the compiler's age.
include(CheckCXXCompilerFlag)
check_cxx_compiler_flag(-Wno-perf-constraint-implies-noexcept HAS_WNO_PERF_CONSTRAINT)
if(HAS_WNO_PERF_CONSTRAINT)
  set(NO_PERF_CONSTRAINT -Wno-perf-constraint-implies-noexcept)
else()
  set(NO_PERF_CONSTRAINT "")
endif()

add_library(summoner_lab_dsp STATIC dsp/src/dsp.cpp)
target_include_directories(summoner_lab_dsp PUBLIC dsp/include)
set_target_properties(summoner_lab_dsp PROPERTIES POSITION_INDEPENDENT_CODE ON)

# The file-access recorder the VST3 tests inject into the process under test.
add_library(summoner_lab_fswatch SHARED fswatch/summoner_lab_fswatch.c)
target_compile_options(summoner_lab_fswatch PRIVATE -Wno-unused-function)
# The allocation counter the no-allocation tests inject the same way.
add_library(summoner_lab_malloccount SHARED malloccount/summoner_lab_malloccount.c)
target_compile_options(summoner_lab_malloccount PRIVATE -Wno-unused-function)
# The AU parameter-list size interposer of the clean-room pins (sizes no AudioUnitSDK unit can produce).
add_library(summoner_lab_plsize SHARED plsize/summoner_lab_plsize.c)
target_link_libraries(summoner_lab_plsize PRIVATE "-framework AudioToolbox")

# The AU registrar probe and the service-lookup recorder. The probe links NOTHING but libSystem
# (CoreFoundation's load closure includes AudioToolbox on macOS 26): it dlopens AudioToolbox after its hold.
add_library(summoner_lab_lookwatch SHARED lookwatch/summoner_lab_lookwatch.c)
target_compile_options(summoner_lab_lookwatch PRIVATE -Wno-unused-function)
# Its private entry points (bootstrap_look_up2 / 3 / _per_user, xpc_pipe_create) are exported by libxpc but absent
# from the SDK's link stubs: dyld binds them at load.
target_link_options(summoner_lab_lookwatch PRIVATE -undefined dynamic_lookup)
add_executable(summoner_au_probe auprobe/summoner_au_probe.c)
target_compile_options(summoner_au_probe PRIVATE -Wno-four-char-constants -Wno-multichar)

enable_testing()
add_executable(summoner_lab_dsp_tests dsp/tests/dsp_tests.cpp)
target_link_libraries(summoner_lab_dsp_tests PRIVATE summoner_lab_dsp)
add_test(NAME summoner_lab_dsp_tests COMMAND summoner_lab_dsp_tests)
# The fswatch recorder's self-test (v2): every interposed call, under the recorder, in the build tree's scratch dir.
add_executable(summoner_lab_fswatch_selftest fswatch/selftest.c)
add_test(NAME summoner_lab_fswatch_selftest COMMAND summoner_lab_fswatch_selftest ${CMAKE_BINARY_DIR})
set_tests_properties(summoner_lab_fswatch_selftest PROPERTIES
  WORKING_DIRECTORY ${CMAKE_BINARY_DIR}
  ENVIRONMENT "DYLD_INSERT_LIBRARIES=$<TARGET_FILE:summoner_lab_fswatch>;SUMMONER_FSWATCH_LOG=${CMAKE_BINARY_DIR}/fswatch-selftest.log")
add_executable(summoner_lab_malloccount_selftest malloccount/selftest.c)
add_test(NAME summoner_lab_malloccount_selftest COMMAND summoner_lab_malloccount_selftest)
set_tests_properties(summoner_lab_malloccount_selftest PROPERTIES
  ENVIRONMENT "DYLD_INSERT_LIBRARIES=$<TARGET_FILE:summoner_lab_malloccount>")

# --- VST3: the cores as VST3 bundles, from the pinned SDK's sources by path -----------------------------------------
# SUMMONER_VST3_SDK_ROOT is tools/vst3-sdk.sh's verified include root (tools/build-plugins.sh passes it). The SDK files
# compiled are exactly this list (VENDORED.md names it too); the SDK's own CMake is never run.
if(DEFINED SUMMONER_VST3_SDK_ROOT)
  set(SDK ${SUMMONER_VST3_SDK_ROOT})
  set(SUMMONER_VST3_PLUGIN_SDK_FILES
    pluginterfaces/base/coreiids.cpp
    pluginterfaces/base/funknown.cpp
    pluginterfaces/base/ustring.cpp
    pluginterfaces/base/conststringtable.cpp
    base/source/baseiids.cpp
    base/source/fobject.cpp
    base/source/fstring.cpp
    base/source/fdebug.cpp
    base/source/fbuffer.cpp
    base/source/fstreamer.cpp
    base/source/updatehandler.cpp
    base/thread/source/flock.cpp
    base/thread/source/fcondition.cpp
    public.sdk/source/common/commoniids.cpp
    public.sdk/source/common/pluginview.cpp
    public.sdk/source/main/pluginfactory.cpp
    public.sdk/source/main/moduleinit.cpp
    public.sdk/source/main/macmain.cpp
    public.sdk/source/vst/vstinitiids.cpp
    public.sdk/source/vst/vstaudioeffect.cpp
    public.sdk/source/vst/vstcomponent.cpp
    public.sdk/source/vst/vstcomponentbase.cpp
    public.sdk/source/vst/vsteditcontroller.cpp
    public.sdk/source/vst/vstbus.cpp
    public.sdk/source/vst/vstparameters.cpp
  )
  list(TRANSFORM SUMMONER_VST3_PLUGIN_SDK_FILES PREPEND "${SDK}/")
  add_library(summoner_vst3_plugin_sdk STATIC ${SUMMONER_VST3_PLUGIN_SDK_FILES})
  target_include_directories(summoner_vst3_plugin_sdk PUBLIC ${SDK})
  target_compile_definitions(summoner_vst3_plugin_sdk PUBLIC RELEASE=1)
  # The SDK's sources are not ours: their warnings are theirs, not errors here.
  target_compile_options(summoner_vst3_plugin_sdk PRIVATE -w)
  set_target_properties(summoner_vst3_plugin_sdk PROPERTIES POSITION_INDEPENDENT_CODE ON)

  foreach(kind gain synth hostile)
    if(kind STREQUAL gain)
      set(n 1)
    elseif(kind STREQUAL synth)
      set(n 2)
    else()
      set(n 3)
    endif()
    set(t summoner-lab-${kind})
    add_library(${t} MODULE vst3/lab_vst3.cpp)
    target_compile_definitions(${t} PRIVATE SUMMONER_LAB_KIND=${n})
    target_link_libraries(${t} PRIVATE summoner_lab_dsp summoner_vst3_plugin_sdk "-framework CoreFoundation")
    set_target_properties(${t} PROPERTIES
      BUNDLE TRUE
      BUNDLE_EXTENSION vst3
      MACOSX_BUNDLE_INFO_PLIST ${CMAKE_CURRENT_SOURCE_DIR}/vst3/Info.plist.in
      MACOSX_BUNDLE_BUNDLE_NAME ${t}
      MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.${kind}
      LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/vst3)
    target_link_options(${t} PRIVATE -exported_symbols_list ${SDK}/public.sdk/source/main/macexport.exp)
  endforeach()

  # The scan fixtures, one bundle each (the hostile bundle's constructor crash would pre-empt any of them):
  # hang = a processor that never returns from its constructor; badstate = one whose getState its own setState refuses;
  # needsnote = a synth (kind 2) that fails its blocks from the 8th on unless a note-on arrived.
  # knobs = the hostile core's knob class alone (no crash class: scannable).
  # statectl = the gain (kind 1) with a controller that keeps its chunk (the state read shows it).
  # ctxmidi = the ProcessContext / MIDI-mapping reporter (kind 2, a synth shell).
  # editor = the gain (kind 1) with an editor view (vst3/lab_view.mm, AppKit).
  # atexit = the gain (kind 1) with a static destructor that dies when armed (a host must `_exit`).
  # titles = the gain (kind 1) plus parameters with non-ASCII titles (UTF-16 to UTF-8).
  # fpcr = the hostile effect (kind 3) reporting the FPU mode (FPCR) its process call ran under.
  # params = the gain (kind 1) with units and the parameter shapes of the parameter tree (order, flags, steps, units).
  # quad = the gain (kind 1) whose main buses are 4 wide (k40Music) and refuse stereo; its process trusts its
  # arrangement (all 4 channels' pointers, in and out), so a host that hands fewer is a fault, not a quiet pass.
  # wide14 / wide41 / wide26 = the quad fixture with 1 in / 4 out, 4 in / 1 out, 2 in / 6 out (asymmetric buses).
  # sidechain / refusesetup = the gain (kind 1): default channels over two buses each way; a refused first
  # setupProcessing (clean-room pins of the VST3 load-time setup and default channel count).
  foreach(fix hang badstate needsnote knobs statectl ctxmidi editor atexit titles fpcr params quad wide14 wide41 wide26
          sidechain refusesetup)
    string(TOUPPER ${fix} FIX)
    set(t summoner-lab-${fix})
    set(kindn 3)
    if(fix STREQUAL needsnote OR fix STREQUAL ctxmidi)
      set(kindn 2)
    endif()
    if(fix STREQUAL statectl OR fix STREQUAL editor OR fix STREQUAL atexit OR fix STREQUAL titles OR fix STREQUAL params
       OR fix STREQUAL quad OR fix MATCHES "^wide" OR fix STREQUAL sidechain OR fix STREQUAL refusesetup)
      set(kindn 1)
    endif()
    add_library(${t} MODULE vst3/lab_vst3.cpp)
    target_compile_definitions(${t} PRIVATE SUMMONER_LAB_KIND=${kindn} SUMMONER_LAB_FIXTURE_${FIX}=1)
    if(fix MATCHES "^wide([0-9])([0-9])$")
      target_compile_definitions(${t} PRIVATE SUMMONER_LAB_FIXTURE_QUAD=1 SUMMONER_LAB_QUAD_IN=${CMAKE_MATCH_1}
        SUMMONER_LAB_QUAD_OUT=${CMAKE_MATCH_2})
    endif()
    if(fix STREQUAL editor)
      target_sources(${t} PRIVATE vst3/lab_view.mm)
      target_include_directories(${t} PRIVATE vst3)
      set_source_files_properties(vst3/lab_view.mm PROPERTIES COMPILE_OPTIONS "-fobjc-arc")
      target_link_libraries(${t} PRIVATE "-framework AppKit")
    endif()
    target_link_libraries(${t} PRIVATE summoner_lab_dsp summoner_vst3_plugin_sdk "-framework CoreFoundation")
    set_target_properties(${t} PROPERTIES
      BUNDLE TRUE
      BUNDLE_EXTENSION vst3
      MACOSX_BUNDLE_INFO_PLIST ${CMAKE_CURRENT_SOURCE_DIR}/vst3/Info.plist.in
      MACOSX_BUNDLE_BUNDLE_NAME ${t}
      MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.${fix}
      LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/vst3)
    target_link_options(${t} PRIVATE -exported_symbols_list ${SDK}/public.sdk/source/main/macexport.exp)
  endforeach()

  # The moduleinfo.json fixtures: the gain, rebuilt, with a moduleinfo.json copied in after the build (never edited
  # by hand). modinfo = a valid one, consistent with the factory but observably its own (version 1.0.1, sub
  # categories Fx|Tools, and the moduleinfo path leaves the channels 0); badmodinfo = Resources/ missing a required
  # key, Contents/ not JSON (their names must never appear: the factory's "Summoner Lab Gain" must); modinfo-mismatch = a
  # valid one naming the class unlike its factory (the pick by name AND id finds no class: failed instantiate).
  foreach(fix modinfo badmodinfo modinfo-mismatch)
    set(t summoner-lab-${fix})
    add_library(${t} MODULE vst3/lab_vst3.cpp)
    target_compile_definitions(${t} PRIVATE SUMMONER_LAB_KIND=1)
    target_link_libraries(${t} PRIVATE summoner_lab_dsp summoner_vst3_plugin_sdk "-framework CoreFoundation")
    set_target_properties(${t} PROPERTIES
      BUNDLE TRUE
      BUNDLE_EXTENSION vst3
      MACOSX_BUNDLE_INFO_PLIST ${CMAKE_CURRENT_SOURCE_DIR}/vst3/Info.plist.in
      MACOSX_BUNDLE_BUNDLE_NAME ${t}
      MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.${fix}
      LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/vst3)
    target_link_options(${t} PRIVATE -exported_symbols_list ${SDK}/public.sdk/source/main/macexport.exp)
    set(dir ${CMAKE_BINARY_DIR}/vst3/${t}.vst3/Contents)
    set(src ${CMAKE_CURRENT_SOURCE_DIR}/vst3/moduleinfo)
    # The copied files are link dependencies, so editing one relinks the bundle and its POST_BUILD copy runs again
    # (a stale moduleinfo.json in a fixture is a wrong fixture).
    set_target_properties(${t} PROPERTIES LINK_DEPENDS
      "${src}/good.json;${src}/mismatch.json;${src}/bad-resources.json;${src}/bad-contents.json")
    if(fix STREQUAL modinfo)
      add_custom_command(TARGET ${t} POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E make_directory ${dir}/Resources
        COMMAND ${CMAKE_COMMAND} -E copy ${src}/good.json ${dir}/Resources/moduleinfo.json)
    elseif(fix STREQUAL modinfo-mismatch)
      add_custom_command(TARGET ${t} POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E make_directory ${dir}/Resources
        COMMAND ${CMAKE_COMMAND} -E copy ${src}/mismatch.json ${dir}/Resources/moduleinfo.json)
    else()
      add_custom_command(TARGET ${t} POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E make_directory ${dir}/Resources
        COMMAND ${CMAKE_COMMAND} -E copy ${src}/bad-resources.json ${dir}/Resources/moduleinfo.json
        COMMAND ${CMAKE_COMMAND} -E copy ${src}/bad-contents.json ${dir}/moduleinfo.json)
    endif()
  endforeach()
endif()

# --- The x86 fixture: an x86_64-only VST3 bundle (no SDK; built, never executed: the scan refuses it by its Mach-O
# header before any load) --------------------------------------------------------------------------------------------
add_library(summoner-lab-x86 MODULE vst3/x86/x86_stub.c)
set_target_properties(summoner-lab-x86 PROPERTIES
  OSX_ARCHITECTURES x86_64
  BUNDLE TRUE
  BUNDLE_EXTENSION vst3
  MACOSX_BUNDLE_INFO_PLIST ${CMAKE_CURRENT_SOURCE_DIR}/vst3/Info.plist.in
  MACOSX_BUNDLE_BUNDLE_NAME summoner-lab-x86
  MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.x86
  LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/vst3)

# --- AU v2: the cores as .component bundles, from the pinned AudioUnitSDK's include/ + src/ by path ----------------
# SUMMONER_AU_SDK_ROOT is tools/au-sdk.sh's verified tree (tools/build-plugins.sh passes it); its own build (Xcode
# project, hooks, tools) is never run. Never installed: target.noindex/plugins/au/ only, registered in-process by the
# host (a lab build never asks the system AU registry).
if(DEFINED SUMMONER_AU_SDK_ROOT)
  set(AUSDK ${SUMMONER_AU_SDK_ROOT})
  set(SUMMONER_AU_SDK_FILES
    AUBase.cpp AUBuffer.cpp AUBufferAllocator.cpp AUEffectBase.cpp AUInputElement.cpp AUMIDIBase.cpp
    AUOutputElement.cpp AUPlugInDispatch.cpp AUScopeElement.cpp ComponentBase.cpp MusicDeviceBase.cpp)
  list(TRANSFORM SUMMONER_AU_SDK_FILES PREPEND "${AUSDK}/src/AudioUnitSDK/")
  add_library(summoner_au_plugin_sdk STATIC ${SUMMONER_AU_SDK_FILES})
  target_include_directories(summoner_au_plugin_sdk SYSTEM PUBLIC ${AUSDK}/include)
  target_compile_options(summoner_au_plugin_sdk PRIVATE -w)
  target_compile_features(summoner_au_plugin_sdk PUBLIC cxx_std_23)
  set_target_properties(summoner_au_plugin_sdk PROPERTIES POSITION_INDEPENDENT_CODE ON CXX_STANDARD 23)

  # <dict> entries of Info.plist's AudioComponents: type subtype factory name [manufacturer] (manufacturer SaSL unless
  # given, version 1.0.0).
  function(summoner_au_component out type sub factory name)
    set(manu SaSL)
    if(ARGC GREATER 5)
      set(manu "${ARGV5}")
    endif()
    set(${out} "${${out}}\t\t<dict>\n\t\t\t<key>type</key><string>${type}</string>\n\t\t\t<key>subtype</key><string>${sub}</string>\n\t\t\t<key>manufacturer</key><string>${manu}</string>\n\t\t\t<key>name</key><string>Summoner Lab: ${name}</string>\n\t\t\t<key>description</key><string>${name}</string>\n\t\t\t<key>version</key><integer>65536</integer>\n\t\t\t<key>factoryFunction</key><string>${factory}</string>\n\t\t</dict>\n" PARENT_SCOPE)
  endfunction()

  # The post-load image check's STOP fixtures: a lab dylib outside every bundle, and a bundle whose executable links it.
  # In lib/, beside the format dirs, the same place as in a pinned export (<sha8>/lib): the drag bundle reaches it by a
  # RELATIVE rpath (@loader_path/../../../../lib from <base>/au/X.component/Contents/MacOS), so a pin is
  # self-contained.
  add_library(summoner_lab_drag SHARED au/drag/drag_lib.c)
  set_target_properties(summoner_lab_drag PROPERTIES LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib)
  set(SUMMONER_AU_COMPONENTS "")
  summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLdr SummonerLabDragFactory "Summoner Lab Drag")
  configure_file(${CMAKE_CURRENT_SOURCE_DIR}/au/Info.plist.in ${CMAKE_BINARY_DIR}/au-plist/drag.plist.in @ONLY)
  add_library(summoner-lab-drag-au MODULE au/drag/drag_bundle.c)
  target_link_libraries(summoner-lab-drag-au PRIVATE summoner_lab_drag)
  set_target_properties(summoner-lab-drag-au PROPERTIES
    BUILD_WITH_INSTALL_RPATH TRUE
    INSTALL_RPATH "@loader_path/../../../../lib"
    BUNDLE TRUE
    BUNDLE_EXTENSION component
    OUTPUT_NAME summoner-lab-drag
    MACOSX_BUNDLE_INFO_PLIST ${CMAKE_BINARY_DIR}/au-plist/drag.plist.in
    MACOSX_BUNDLE_BUNDLE_NAME summoner-lab-drag
    MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.au.drag
    LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/au)

  foreach(kind gain synth hostile)
    set(SUMMONER_AU_COMPONENTS "")
    if(kind STREQUAL gain)
      set(n 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLgn SummonerLabGainFactory "Summoner Lab Gain")
    elseif(kind STREQUAL synth)
      set(n 2)
      summoner_au_component(SUMMONER_AU_COMPONENTS aumu SLsy SummonerLabSynthFactory "Summoner Lab Synth")
    else()
      set(n 3)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLho SummonerLabHostileFactory "Summoner Lab Hostile")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh1 SummonerLabHostileCtorCrashFactory "Summoner Lab Hostile (crash in constructor)")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh2 SummonerLabHostilePrepareCrashFactory "Summoner Lab Hostile (crash in prepare)")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh3 SummonerLabHostileSlowPrepareFactory "Summoner Lab Hostile (3 s prepare)")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh7 SummonerLabHostileLiarFactory "Summoner Lab Hostile (77 parameters)")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh5 SummonerLabHostileDeafFactory "Summoner Lab Hostile (refuses listeners)")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh8 SummonerLabHostileDragCtorFactory "Summoner Lab Hostile (loads a dylib in its constructor)")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLh9 SummonerLabHostileDragInitFactory "Summoner Lab Hostile (loads a dylib in Initialize)")
    endif()
    set(t summoner-lab-${kind}-au)
    configure_file(${CMAKE_CURRENT_SOURCE_DIR}/au/Info.plist.in ${CMAKE_BINARY_DIR}/au-plist/${kind}.plist.in @ONLY)
    # The exported symbols: exactly this bundle's factories.
    string(REGEX MATCHALL "SummonerLab[A-Za-z]*Factory" fns "${SUMMONER_AU_COMPONENTS}")
    list(TRANSFORM fns PREPEND "_")
    string(REPLACE ";" "\n" fns "${fns}")
    file(WRITE ${CMAKE_BINARY_DIR}/au-plist/${kind}.exp "${fns}\n")
    add_library(${t} MODULE au/lab_au.cpp)
    target_compile_definitions(${t} PRIVATE SUMMONER_LAB_KIND=${n})
    if(n EQUAL 3)
      # The drag dylib's name; the hostile AU finds it beside its own bundle at run time (../../../lib), so a pin is
      # self-contained.
      target_compile_definitions(${t} PRIVATE SUMMONER_LAB_DRAG_NAME="libsummoner_lab_drag.dylib")
      add_dependencies(${t} summoner_lab_drag)
    endif()
    set_target_properties(${t} PROPERTIES CXX_STANDARD 23)
    # The SDK marks its realtime virtuals nonblocking (AUSDK_RTSAFE); the hostile core throws from process on purpose
    # (std::terminate inside the SDK's noexcept DoRender), so the overrides are not noexcept.
    target_compile_options(${t} PRIVATE ${NO_PERF_CONSTRAINT})
    target_link_libraries(${t} PRIVATE summoner_lab_dsp summoner_au_plugin_sdk
      "-framework AudioToolbox" "-framework CoreFoundation" "-framework CoreAudio")
    set_target_properties(${t} PROPERTIES
      BUNDLE TRUE
      BUNDLE_EXTENSION component
      OUTPUT_NAME summoner-lab-${kind}
      MACOSX_BUNDLE_INFO_PLIST ${CMAKE_BINARY_DIR}/au-plist/${kind}.plist.in
      MACOSX_BUNDLE_BUNDLE_NAME summoner-lab-${kind}
      MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.au.${kind}
      LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/au)
    target_link_options(${t} PRIVATE -exported_symbols_list ${CMAKE_BINARY_DIR}/au-plist/${kind}.exp)
  endforeach()

  # The scan fixtures as AU, one bundle each (as the VST3 ones above); and two of the gain core: paramlist = a
  # ParameterList that changes at a block count, hostcb = a reporter of the host's HostCallbacks.
  # cocoaui = the gain with a Cocoa UI (kAudioUnitProperty_CocoaUI, au/cocoa_view.mm).
  # offline = the gain storing kAudioUnitProperty_OfflineRender; multi = two entries whose uniqueIds (the XOR)
  # collide, SLxa/SaSL and SLx`/SaSM; badcodes = one good entry and four code refusals: a ':' in the subtype, a tab in
  # the manufacturer, a non-ASCII byte (UTF-8 e-acute) in the subtype, a ',' in the type.
  # badorder = a code refusal (a ':' in the subtype) FIRST, then an entry whose validation fails (it refuses its own
  # state), then a good one: the validation failure, not the refusal, is the bundle's verdict.
  # params = the gain with parameters that exercise the AU parameter derivations (clumps, NonRealTime, units, steps).
  # synthin = the synth (a music device) with one input element, adding what it pulls to its output;
  # synthinstrict = the same, its input element refusing a stereo stream format (the host's 0-input retry).
  # paramsfixed = params with every name in the fixed `name` array, no CF names (clean-room pins of the AU name path).
  foreach(fix hang badstate needsnote paramlist hostcb cocoaui offline multi badcodes badorder params synthin
          synthinstrict synthinchange paramsfixed)
    string(TOUPPER ${fix} FIX)
    set(SUMMONER_AU_COMPONENTS "")
    set(kindn 3)
    if(fix STREQUAL hang)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLhg SummonerLabHangFactory "Summoner Lab Hang (hangs in constructor)")
    elseif(fix STREQUAL badstate)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLbs SummonerLabBadStateFactory "Summoner Lab Bad State (refuses its own state)")
    elseif(fix STREQUAL needsnote)
      set(kindn 2)
      summoner_au_component(SUMMONER_AU_COMPONENTS aumu SLnn SummonerLabNeedsNoteFactory "Summoner Lab Needs Note (fails without a note)")
    elseif(fix STREQUAL synthin)
      set(kindn 2)
      summoner_au_component(SUMMONER_AU_COMPONENTS aumu SLsi SummonerLabSynthInFactory "Summoner Lab Synth In (a music device with an input)")
    elseif(fix STREQUAL synthinstrict)
      set(kindn 2)
      summoner_au_component(SUMMONER_AU_COMPONENTS aumu SLsx SummonerLabSynthInStrictFactory "Summoner Lab Synth In Strict (refuses stereo input)")
    elseif(fix STREQUAL synthinchange)
      set(kindn 2)
      summoner_au_component(SUMMONER_AU_COMPONENTS aumu SLsc SummonerLabSynthInChangeFactory "Summoner Lab Synth In Change (gain controls input refusal)")
    elseif(fix STREQUAL paramlist)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLpl SummonerLabParamListFactory "Summoner Lab Param List (changes at block 8)")
    elseif(fix STREQUAL hostcb)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLhc SummonerLabHostCbFactory "Summoner Lab Host Callbacks (reports them)")
    elseif(fix STREQUAL cocoaui)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLcu SummonerLabCocoaUiFactory "Summoner Lab Cocoa UI")
    elseif(fix STREQUAL offline)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLof SummonerLabOfflineFactory "Summoner Lab Offline (stores OfflineRender)")
    elseif(fix STREQUAL multi)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLxa SummonerLabMultiAFactory "Summoner Lab Multi A" SaSL)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx "SLx`" SummonerLabMultiBFactory "Summoner Lab Multi B (77 parameters)" SaSM)
    elseif(fix STREQUAL badcodes)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLc0 SummonerLabBadCodesFactory "Summoner Lab Codes OK")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx "S:c1" SummonerLabBadCodesFactory "Summoner Lab Codes Colon")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLc2 SummonerLabBadCodesFactory "Summoner Lab Codes Tab" "Sa\tL")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx "SLé" SummonerLabBadCodesFactory "Summoner Lab Codes Latin")
      summoner_au_component(SUMMONER_AU_COMPONENTS "au,x" SLc4 SummonerLabBadCodesFactory "Summoner Lab Codes Comma")
    elseif(fix STREQUAL params)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLpm SummonerLabParamsFactory "Summoner Lab Params")
    elseif(fix STREQUAL paramsfixed)
      set(kindn 1)
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLpf SummonerLabParamsFactory "Summoner Lab Params Fixed")
    elseif(fix STREQUAL badorder)
      # The hostile core (kind 3, as badstate): only it honours refusesOwnState.
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx "S:o0" SummonerLabBadOrderOkFactory "Summoner Lab Order Colon")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLo1 SummonerLabBadOrderStateFactory "Summoner Lab Order Bad State")
      summoner_au_component(SUMMONER_AU_COMPONENTS aufx SLo2 SummonerLabBadOrderOkFactory "Summoner Lab Order OK")
    endif()
    set(t summoner-lab-${fix}-au)
    configure_file(${CMAKE_CURRENT_SOURCE_DIR}/au/Info.plist.in ${CMAKE_BINARY_DIR}/au-plist/${fix}.plist.in @ONLY)
    string(REGEX MATCHALL "SummonerLab[A-Za-z]*Factory" fns "${SUMMONER_AU_COMPONENTS}")
    list(REMOVE_DUPLICATES fns)
    list(TRANSFORM fns PREPEND "_")
    string(REPLACE ";" "\n" fns "${fns}")
    file(WRITE ${CMAKE_BINARY_DIR}/au-plist/${fix}.exp "${fns}\n")
    add_library(${t} MODULE au/lab_au.cpp)
    target_compile_definitions(${t} PRIVATE SUMMONER_LAB_KIND=${kindn} SUMMONER_LAB_FIXTURE_${FIX}=1)
    if(fix STREQUAL paramsfixed)
      target_compile_definitions(${t} PRIVATE SUMMONER_LAB_FIXTURE_PARAMS=1)
    endif()
    if(fix STREQUAL cocoaui)
      target_sources(${t} PRIVATE au/cocoa_view.mm)
      set_source_files_properties(au/cocoa_view.mm PROPERTIES COMPILE_OPTIONS "-fobjc-arc")
      target_link_libraries(${t} PRIVATE "-framework AppKit")
    endif()
    set_target_properties(${t} PROPERTIES CXX_STANDARD 23)
    target_compile_options(${t} PRIVATE ${NO_PERF_CONSTRAINT})
    target_link_libraries(${t} PRIVATE summoner_lab_dsp summoner_au_plugin_sdk
      "-framework AudioToolbox" "-framework CoreFoundation" "-framework CoreAudio")
    set_target_properties(${t} PROPERTIES
      BUNDLE TRUE
      BUNDLE_EXTENSION component
      OUTPUT_NAME summoner-lab-${fix}
      MACOSX_BUNDLE_INFO_PLIST ${CMAKE_BINARY_DIR}/au-plist/${fix}.plist.in
      MACOSX_BUNDLE_BUNDLE_NAME summoner-lab-${fix}
      MACOSX_BUNDLE_GUI_IDENTIFIER org.summoner.testkit.au.${fix}
      LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/au)
    target_link_options(${t} PRIVATE -exported_symbols_list ${CMAKE_BINARY_DIR}/au-plist/${fix}.exp)
  endforeach()
endif()
